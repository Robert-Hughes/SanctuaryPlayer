use ::oxideav::core::{
    CancellationToken, CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecTag, Decoder,
    Error, ExecutionContext, Frame, Packet, Result,
};

/// Register Sanctuary's Windows Vulkan Video H.264 adapter.
///
/// The upstream decoder already performs GPU decode followed by an NV12 staging
/// readback, but its current CPU VideoFrame output does not carry packet PTS.
/// Sanctuary requires timestamps for presentation, so the adapter preserves the
/// current packet PTS while leaving the decoded plane storage untouched.
pub fn register(ctx: &mut ::oxideav::Registries) {
    let capabilities = CodecCapabilities::video("h264_vulkan")
        .with_lossy(true)
        .with_intra_only(false)
        .with_hardware(true)
        .with_priority(20);

    ctx.codecs.register(
        CodecInfo::new(CodecId::new("h264"))
            .capabilities(capabilities.with_decode())
            .decoder(make_decoder)
            .tags([
                CodecTag::fourcc(b"H264"),
                CodecTag::fourcc(b"h264"),
                CodecTag::fourcc(b"AVC1"),
                CodecTag::fourcc(b"avc1"),
                CodecTag::fourcc(b"X264"),
                CodecTag::matroska("V_MPEG4/ISO/AVC"),
            ])
            .with_engine_id("vulkan-video")
            .with_engine_probe(oxideav_vulkan_video::engine_info),
    );
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
