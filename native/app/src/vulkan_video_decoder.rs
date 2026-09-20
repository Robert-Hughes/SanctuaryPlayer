use std::sync::{OnceLock, RwLock};

use ::oxideav::core::{
    CancellationToken, CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecTag, Decoder,
    Error, ExecutionContext, Frame, Packet, Result,
};
use ash::vk::{self, Handle};
use oxideav_vulkan_video::ExternalDevice;
use wgpu::hal::api::Vulkan;

const VIDEO_QUEUE_PRIORITY: [f32; 1] = [1.0];
const SHARED_QUEUE_PRIORITIES: [f32; 2] = [1.0, 1.0];

static DIRECT_DEVICE: OnceLock<RwLock<Option<ExternalDevice>>> = OnceLock::new();

fn direct_device_slot() -> &'static RwLock<Option<ExternalDevice>> {
    DIRECT_DEVICE.get_or_init(|| RwLock::new(None))
}

pub(crate) fn install_direct_device(device: ExternalDevice) {
    if let Ok(mut slot) = direct_device_slot().write() {
        *slot = Some(device);
    }
}

pub(crate) fn clear_direct_device() {
    if let Ok(mut slot) = direct_device_slot().write() {
        *slot = None;
    }
}

fn direct_device() -> Result<ExternalDevice> {
    direct_device_slot()
        .read()
        .map_err(|_| Error::other("vulkan-video: shared-device registry is poisoned"))?
        .as_ref()
        .copied()
        .ok_or_else(|| {
            Error::unsupported(
                "vulkan-video: direct presentation requires Sanctuary's shared wgpu Vulkan device",
            )
        })
}

pub(crate) fn request_shared_wgpu_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> std::result::Result<(wgpu::Device, wgpu::Queue), String> {
    let hal_adapter = unsafe { adapter.as_hal::<Vulkan>() }
        .ok_or_else(|| "vulkan-direct requires the wgpu Vulkan backend".to_string())?;
    let physical_device = hal_adapter.raw_physical_device();
    let raw_instance = hal_adapter.shared_instance().raw_instance();
    let queue_families =
        unsafe { raw_instance.get_physical_device_queue_family_properties(physical_device) };
    let video_queue_family_index = queue_families
        .iter()
        .enumerate()
        .find(|(_, props)| props.queue_flags.contains(vk::QueueFlags::VIDEO_DECODE_KHR))
        .map(|(index, _)| index as u32)
        .ok_or_else(|| {
            "vulkan-direct: selected GPU has no Vulkan Video decode queue".to_string()
        })?;

    let required_extensions = [
        ash::khr::video_queue::NAME,
        ash::khr::video_decode_queue::NAME,
        ash::khr::video_decode_h264::NAME,
    ];
    let available_extensions = unsafe {
        raw_instance
            .enumerate_device_extension_properties(physical_device)
            .map_err(|error| format!("vulkan-direct enumerate device extensions: {error}"))?
    };
    for required in required_extensions {
        let available = available_extensions.iter().any(|property| unsafe {
            std::ffi::CStr::from_ptr(property.extension_name.as_ptr()) == required
        });
        if !available {
            return Err(format!(
                "vulkan-direct: selected GPU is missing {}",
                required.to_string_lossy()
            ));
        }
    }
    let synchronization2_available = available_extensions.iter().any(|property| unsafe {
        std::ffi::CStr::from_ptr(property.extension_name.as_ptr())
            == ash::khr::synchronization2::NAME
    });

    let graphics_queue_family_index = 0u32;
    let video_queue_index = if video_queue_family_index == graphics_queue_family_index {
        if queue_families[video_queue_family_index as usize].queue_count < 2 {
            return Err(
                "vulkan-direct: graphics/video queue family has only one queue; a dedicated decode queue is required"
                    .to_string(),
            );
        }
        1
    } else {
        0
    };

    let callback = Box::new(
        move |args: wgpu::hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>| {
            for extension in required_extensions {
                if !args.extensions.contains(&extension) {
                    args.extensions.push(extension);
                }
            }
            if synchronization2_available
                && !args.extensions.contains(&ash::khr::synchronization2::NAME)
            {
                args.extensions.push(ash::khr::synchronization2::NAME);
            }
            if video_queue_family_index == graphics_queue_family_index {
                if let Some(info) = args
                    .queue_create_infos
                    .iter_mut()
                    .find(|info| info.queue_family_index == graphics_queue_family_index)
                {
                    *info = vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(graphics_queue_family_index)
                        .queue_priorities(&SHARED_QUEUE_PRIORITIES);
                }
            } else {
                args.queue_create_infos.push(
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(video_queue_family_index)
                        .queue_priorities(&VIDEO_QUEUE_PRIORITY),
                );
            }
        },
    );

    let open_device = unsafe {
        hal_adapter.open_with_callback(
            desc.required_features,
            &desc.required_limits,
            &desc.memory_hints,
            Some(callback),
        )
    }
    .map_err(|error| format!("vulkan-direct create shared Vulkan device: {error:?}"))?;
    drop(hal_adapter);

    let (device, queue) = unsafe { adapter.create_device_from_hal::<Vulkan>(open_device, desc) }
        .map_err(|error| format!("vulkan-direct create wgpu device from Vulkan HAL: {error}"))?;

    let hal_device = unsafe { device.as_hal::<Vulkan>() }
        .ok_or_else(|| "vulkan-direct: shared device was not Vulkan".to_string())?;
    let graphics_queue_family_index = hal_device.queue_family_index();
    let external = ExternalDevice::new(
        hal_device
            .shared_instance()
            .raw_instance()
            .handle()
            .as_raw() as usize as *mut std::ffi::c_void,
        hal_device.raw_physical_device().as_raw() as usize as *mut std::ffi::c_void,
        hal_device.raw_device().handle().as_raw() as usize as *mut std::ffi::c_void,
        video_queue_family_index,
    )
    .with_queue_index(video_queue_index)
    .with_consumer_queue_family_index(graphics_queue_family_index);
    install_direct_device(external);
    log::info!(
        "SanctuaryPlayer: shared Vulkan device direct-video queues graphics_family={} decode_family={} decode_queue={}",
        graphics_queue_family_index,
        video_queue_family_index,
        video_queue_index
    );
    drop(hal_device);
    Ok((device, queue))
}

/// Register Sanctuary's Windows Vulkan Video H.264 adapters.
///
/// Readback remains the higher-priority hardware implementation so automatic
/// selection is unchanged. The direct implementation is selected only by an
/// explicit vulkan-direct request.
pub fn register(ctx: &mut ::oxideav::Registries) {
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("h264"))
            .capabilities(
                CodecCapabilities::video("h264_vulkan")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_hardware(true)
                    .with_priority(20)
                    .with_decode(),
            )
            .decoder(make_decoder)
            .tags(h264_tags())
            .with_engine_id("vulkan-video")
            .with_engine_probe(oxideav_vulkan_video::engine_info),
    );

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("h264"))
            .capabilities(
                CodecCapabilities::video("h264_vulkan_direct")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_hardware(true)
                    .with_priority(19)
                    .with_decode(),
            )
            .decoder(make_direct_decoder)
            .tags(h264_tags())
            .with_engine_id("vulkan-video-direct")
            .with_engine_probe(oxideav_vulkan_video::engine_info),
    );
}

fn h264_tags() -> [CodecTag; 6] {
    [
        CodecTag::fourcc(b"H264"),
        CodecTag::fourcc(b"h264"),
        CodecTag::fourcc(b"AVC1"),
        CodecTag::fourcc(b"avc1"),
        CodecTag::fourcc(b"X264"),
        CodecTag::matroska("V_MPEG4/ISO/AVC"),
    ]
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    // H264VkDecoder::make() only probes the Vulkan loader; its expensive
    // device/session validation is deferred until the first SPS/PPS packet.
    // Probe the requested device up front so OxideAV can still fall back to
    // h264_sw during factory selection on GPUs without Vulkan Video H.264.
    let device_index = params.device_index.unwrap_or(0) as usize;
    let devices = oxideav_vulkan_video::engine_info();
    let device = devices.get(device_index).ok_or_else(|| {
        Error::unsupported(format!(
            "vulkan-video: device_index {device_index} is unavailable"
        ))
    })?;
    if !device
        .codecs
        .iter()
        .any(|codec| codec.codec == "h264" && codec.decode)
    {
        return Err(Error::unsupported(format!(
            "vulkan-video: {} does not advertise H.264 decode",
            device.name
        )));
    }

    log::info!(
        "SanctuaryPlayer: selecting Vulkan Video H.264 decoder on {}",
        device.name
    );
    let inner = oxideav_vulkan_video::decoder::H264VkDecoder::make(params)?;
    Ok(Box::new(TimestampedVulkanDecoder {
        inner,
        current_packet_pts: None,
    }))
}

fn make_direct_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let device_index = params.device_index.unwrap_or(0) as usize;
    let devices = oxideav_vulkan_video::engine_info();
    let device = devices.get(device_index).ok_or_else(|| {
        Error::unsupported(format!(
            "vulkan-video: device_index {device_index} is unavailable"
        ))
    })?;
    if !device
        .codecs
        .iter()
        .any(|codec| codec.codec == "h264" && codec.decode)
    {
        return Err(Error::unsupported(format!(
            "vulkan-video: {} does not advertise H.264 decode",
            device.name
        )));
    }

    let external = direct_device()?;
    log::info!(
        "SanctuaryPlayer: selecting direct Vulkan Video H.264 decoder on shared wgpu device {}",
        device.name
    );
    // SAFETY: Graphics installs handles from its live wgpu-owned Vulkan device
    // and keeps that device alive for the application's playback lifetime.
    unsafe {
        oxideav_vulkan_video::decoder::H264VkDecoder::make_direct_with_device(params, external)
    }
}

struct TimestampedVulkanDecoder {
    inner: Box<dyn Decoder>,
    current_packet_pts: Option<i64>,
}

impl Decoder for TimestampedVulkanDecoder {
    fn codec_id(&self) -> &CodecId {
        self.inner.codec_id()
    }

    fn output_params(&self) -> Option<&CodecParameters> {
        self.inner.output_params()
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.current_packet_pts = packet.pts;
        self.inner.send_packet(packet)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let frame = self.inner.receive_frame()?;
        Ok(stamp_missing_video_pts(frame, self.current_packet_pts))
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }

    fn reset(&mut self) -> Result<()> {
        self.current_packet_pts = None;
        self.inner.reset()
    }

    fn set_execution_context(&mut self, ctx: &ExecutionContext) {
        self.inner.set_execution_context(ctx);
    }

    fn set_cancellation_token(&mut self, token: CancellationToken) {
        self.inner.set_cancellation_token(token);
    }
}

fn stamp_missing_video_pts(mut frame: Frame, pts: Option<i64>) -> Frame {
    if let Frame::Video(video) = &mut frame
        && video.pts.is_none()
    {
        video.pts = pts;
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::oxideav::core::{VideoFrame, VideoPlane};

    #[test]
    fn vulkan_adapter_preserves_existing_pts_and_fills_missing_pts() {
        let frame = Frame::Video(VideoFrame {
            pts: None,
            planes: vec![VideoPlane {
                stride: 2,
                data: vec![0; 4],
            }],
        });
        assert_eq!(stamp_missing_video_pts(frame, Some(123)).pts(), Some(123));

        let frame = Frame::Video(VideoFrame {
            pts: Some(456),
            planes: vec![VideoPlane {
                stride: 2,
                data: vec![0; 4],
            }],
        });
        assert_eq!(stamp_missing_video_pts(frame, Some(123)).pts(), Some(456));
    }
}
