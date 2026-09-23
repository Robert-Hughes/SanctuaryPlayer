use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use ::oxideav::core::{AudioFrame, CodecParameters, TimeBase};
use oxideav_audio_filter::{
    AudioFilter, AudioStreamParams, resample::Resample, sample_convert::decode_to_f32,
};
use oxideav_sysaudio::{
    self as sysaudio, ContentType, Driver, StreamFormat, StreamRequest, StreamUsage,
};

use crate::audio_timeline::{PcmTimelineProducer, QueueResult, pcm_timeline_ring};

const RING_SECONDS: usize = 4;
const PREROLL_MILLIS: u64 = 50;

/// Sanctuary-owned decoded-audio output. Decoded `AudioFrame`s are converted
/// to interleaved f32 PCM and merged into a timestamp-aware bounded ring on the
/// application thread. The platform audio callback consumes exactly the PTS it
/// needs next, dropping stale PCM and inserting silence for unavailable time.
pub(crate) struct AudioOutput {
    stream: sysaudio::Stream,
    producer: PcmTimelineProducer,
    submitted_samples: Arc<AtomicU64>,
    underrun_callbacks: Arc<AtomicU64>,
    underrun_samples: Arc<AtomicU64>,
    callback_active: Arc<AtomicBool>,
    source_params: AudioStreamParams,
    source_time_base: TimeBase,
    resampler: Option<Resample>,
    conversion_origin: Option<i64>,
    converted_input_samples: i64,
    converted_output_samples: i64,
    pending_pcm: VecDeque<PcmBlock>,
    input_finished: bool,
    device_rate: u32,
    media_origin: Option<Duration>,
    preroll_target_samples: u64,
    preroll_done: bool,
    user_paused: bool,
    backend_name: &'static str,
}

struct PcmBlock {
    pts: Option<i64>,
    samples: u64,
    interleaved: Vec<f32>,
}

impl AudioOutput {
    pub(crate) fn open(
        params: &CodecParameters,
        time_base: TimeBase,
        muted: bool,
    ) -> Result<Self, String> {
        let driver = sysaudio::default_driver()
            .ok_or_else(|| "oxideav-sysaudio: no usable audio output backend".to_owned())?;
        Self::open_with_driver(driver, params, time_base, muted)
    }

    pub(crate) fn open_with_driver(
        driver: Driver,
        params: &CodecParameters,
        time_base: TimeBase,
        muted: bool,
    ) -> Result<Self, String> {
        let source_rate = params
            .sample_rate
            .filter(|rate| *rate > 0)
            .ok_or_else(|| "decoded audio stream has no sample rate".to_owned())?;
        let source_channels = params
            .resolved_channels()
            .filter(|channels| *channels > 0)
            .ok_or_else(|| "decoded audio stream has no channel count".to_owned())?;
        let source_format = params
            .sample_format
            .ok_or_else(|| "decoded audio stream has no sample format".to_owned())?;
        if !time_base.is_valid() {
            return Err("decoded audio stream has no valid time base".to_owned());
        }
        let source_params = AudioStreamParams {
            format: source_format,
            channels: source_channels,
            sample_rate: source_rate,
        };

        let requested_rate = driver
            .preferred_format(None)
            .ok()
            .flatten()
            .map(|format| format.sample_rate)
            .filter(|rate| *rate > 0)
            .unwrap_or(source_rate);
        let capacity_frames = (requested_rate as usize)
            .saturating_mul(RING_SECONDS)
            .max(4096);
        let (producer, mut consumer) = pcm_timeline_ring(capacity_frames, source_channels);
        let submitted_samples = Arc::new(AtomicU64::new(0));
        let submitted_samples_cb = Arc::clone(&submitted_samples);
        let underrun_callbacks = Arc::new(AtomicU64::new(0));
        let underrun_callbacks_cb = Arc::clone(&underrun_callbacks);
        let underrun_samples = Arc::new(AtomicU64::new(0));
        let underrun_samples_cb = Arc::clone(&underrun_samples);
        let callback_active = Arc::new(AtomicBool::new(false));
        let callback_active_cb = Arc::clone(&callback_active);

        let request = StreamRequest::new(requested_rate, source_channels)
            .with_usage(StreamUsage::Media)
            .with_content_type(ContentType::Movie);
        let mut stream = sysaudio::open(driver, request, move |out, _info| {
            let stats = consumer.fill(out);
            if callback_active_cb.load(Ordering::Relaxed) {
                submitted_samples_cb.fetch_add(stats.output_frames, Ordering::Relaxed);
                if stats.silence_frames != 0 {
                    underrun_callbacks_cb.fetch_add(1, Ordering::Relaxed);
                    underrun_samples_cb.fetch_add(stats.silence_frames, Ordering::Relaxed);
                }
            }
        })
        .map_err(|error| format!("oxideav-sysaudio {} open failed: {error}", driver.name()))?;

        // Streams start immediately. Keep the device paused until Sanctuary has
        // enough decoded PCM for a small preroll and the user actually presses
        // Play. A racing callback before this pause receives silence but cannot
        // advance the uninitialised audio timeline.
        stream
            .pause()
            .map_err(|error| format!("oxideav-sysaudio {} pause failed: {error}", driver.name()))?;

        if muted {
            stream.set_volume(0.0);
        }

        let device = stream.format();
        validate_device_format(source_channels, device, driver.name())?;
        let resampler = (source_rate != device.sample_rate)
            .then(|| Resample::new(source_rate, device.sample_rate))
            .transpose()
            .map_err(|error| format!("create audio resampler: {error}"))?;
        let preroll_target_samples = ((device.sample_rate as u64) * PREROLL_MILLIS / 1000).max(1);

        log::info!(
            "SanctuaryPlayer: audio output sysaudio/{} source={}Hz {}ch {:?} device={}Hz {}ch {:?} preroll={}ms muted={muted}",
            driver.name(),
            source_rate,
            source_channels,
            source_format,
            device.sample_rate,
            device.channels,
            device.format,
            PREROLL_MILLIS,
        );

        Ok(Self {
            stream,
            producer,
            submitted_samples,
            underrun_callbacks,
            underrun_samples,
            callback_active,
            source_params,
            source_time_base: time_base,
            resampler,
            conversion_origin: None,
            converted_input_samples: 0,
            converted_output_samples: 0,
            pending_pcm: VecDeque::new(),
            input_finished: false,
            device_rate: device.sample_rate,
            media_origin: None,
            preroll_target_samples,
            preroll_done: false,
            user_paused: true,
            backend_name: driver.name(),
        })
    }

    pub(crate) fn set_media_origin(&mut self, origin: Duration) -> Result<(), String> {
        if self.media_origin.is_none() {
            log::info!(
                "SanctuaryPlayer: audio clock anchored at {:.3}s",
                origin.as_secs_f64()
            );
            self.media_origin = Some(origin);
        }
        self.apply_play_state()
    }

    pub(crate) fn queue(&mut self, frame: &AudioFrame) -> Result<QueueResult, String> {
        // Deferred calls retry the same frame. Preserve converted PCM so the
        // streaming resampler advances exactly once, even under backpressure.
        if self.pending_pcm.is_empty() {
            if self.resampler.is_some() {
                let source_clock = TimeBase::from_rate(self.source_params.sample_rate);
                let input_pts = frame
                    .pts
                    .map(|pts| {
                        self.source_time_base
                            .rescale_checked(pts, source_clock)
                            .ok_or_else(|| "cannot rescale audio PTS".to_owned())
                    })
                    .transpose()?;
                let tolerance = self
                    .source_time_base
                    .rescale_checked(1, source_clock)
                    .unwrap_or(1)
                    .abs()
                    .max(1);
                if let Some(pts) = input_pts {
                    let discontinuity = self.conversion_origin.is_none_or(|origin| {
                        pts.abs_diff(origin.saturating_add(self.converted_input_samples))
                            > tolerance as u64
                    });
                    if discontinuity {
                        self.resampler = Some(
                            Resample::new(self.source_params.sample_rate, self.device_rate)
                                .map_err(|error| format!("reset audio resampler: {error}"))?,
                        );
                        self.conversion_origin = Some(pts);
                        self.converted_input_samples = 0;
                        self.converted_output_samples = 0;
                    }
                }
                let frames = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .process(frame, self.source_params)
                    .map_err(|error| format!("resample decoded audio: {error}"))?;
                self.converted_input_samples += i64::from(frame.samples);
                self.append_converted(frames)?;
            } else {
                let pts = frame
                    .pts
                    .map(|pts| {
                        self.source_time_base
                            .rescale_checked(pts, TimeBase::from_rate(self.device_rate))
                            .ok_or_else(|| "cannot rescale audio PTS".to_owned())
                    })
                    .transpose()?;
                self.pending_pcm
                    .push_back(pcm_block(frame, self.source_params, pts)?);
            }
        }
        let result = self.drain_pending()?;
        self.maybe_finish_preroll()?;
        Ok(result)
    }

    fn append_converted(&mut self, frames: Vec<AudioFrame>) -> Result<(), String> {
        for frame in frames {
            // Anchor once, then advance by the actual output count. Rescaling
            // each AAC frame independently would introduce rounding gaps.
            let pts = self
                .conversion_origin
                .map(|origin| {
                    TimeBase::from_rate(self.source_params.sample_rate)
                        .rescale_checked(origin, TimeBase::from_rate(self.device_rate))
                        .and_then(|origin| origin.checked_add(self.converted_output_samples))
                        .ok_or_else(|| "converted audio PTS overflow".to_owned())
                })
                .transpose()?;
            self.converted_output_samples += i64::from(frame.samples);
            self.pending_pcm
                .push_back(pcm_block(&frame, self.source_params, pts)?);
        }
        Ok(())
    }

    fn drain_pending(&mut self) -> Result<QueueResult, String> {
        let mut result = QueueResult::Dropped;
        while let Some(block) = self.pending_pcm.front() {
            let queued =
                self.producer
                    .queue_interleaved(block.pts, block.samples, &block.interleaved)?;
            if queued == QueueResult::Deferred {
                return Ok(queued);
            }
            if queued == QueueResult::Queued {
                result = queued;
            }
            self.pending_pcm.pop_front();
        }
        Ok(result)
    }

    pub(crate) fn finish_input(&mut self) -> Result<(), String> {
        if !self.input_finished {
            if let Some(resampler) = &mut self.resampler {
                let frames = resampler
                    .flush(self.source_params)
                    .map_err(|error| format!("flush audio resampler: {error}"))?;
                self.append_converted(frames)?;
            }
            self.input_finished = true;
        }
        self.drain_pending()?;
        // EOF is also a preroll boundary: a clip shorter than the normal
        // target must still be allowed to start and drain.
        self.preroll_done = true;
        self.apply_play_state()
    }

    fn maybe_finish_preroll(&mut self) -> Result<(), String> {
        if !self.preroll_done && self.queued_samples() >= self.preroll_target_samples {
            self.preroll_done = true;
            log::info!(
                "SanctuaryPlayer: audio preroll ready queued={:.1}ms",
                self.queued_duration().as_secs_f64() * 1000.0
            );
            self.apply_play_state()?;
        }
        Ok(())
    }

    pub(crate) fn set_paused(&mut self, paused: bool) -> Result<(), String> {
        self.user_paused = paused;
        self.apply_play_state()
    }

    fn apply_play_state(&mut self) -> Result<(), String> {
        let should_play = self.preroll_done
            && self.media_origin.is_some()
            && self.producer.origin_pts().is_some()
            && !self.user_paused;
        if should_play == self.stream.is_playing() {
            return Ok(());
        }
        log::info!(
            "SanctuaryPlayer: audio stream {} queued={:.1}ms submitted={:.3}s next_output_pts={:?}",
            if should_play { "playing" } else { "paused" },
            self.queued_duration().as_secs_f64() * 1000.0,
            duration_from_samples(self.submitted_samples(), self.device_rate).as_secs_f64(),
            self.producer.next_output_pts(),
        );
        self.callback_active.store(should_play, Ordering::Relaxed);
        let result = if should_play {
            self.stream.play()
        } else {
            self.stream.pause()
        };
        if result.is_err() {
            self.callback_active.store(false, Ordering::Relaxed);
        }
        result.map_err(|error| {
            format!(
                "oxideav-sysaudio {} {} failed: {error}",
                self.backend_name,
                if should_play { "play" } else { "pause" }
            )
        })
    }

    pub(crate) fn set_volume(&self, volume: f32) {
        self.stream.set_volume(volume);
    }

    pub(crate) fn preroll_ready(&self) -> bool {
        self.preroll_done
    }

    pub(crate) fn queued_samples(&self) -> u64 {
        self.producer.queued_frames() as u64
            + self
                .pending_pcm
                .iter()
                .map(|block| block.samples)
                .sum::<u64>()
    }

    pub(crate) fn headroom_samples(&self) -> u64 {
        self.producer.vacant_frames() as u64
    }

    pub(crate) fn queue_target_samples(&self) -> u64 {
        (self.device_rate as u64 / 2).max(1)
    }

    pub(crate) fn minimum_headroom_samples(&self) -> u64 {
        (self.device_rate as u64 / 10).max(1)
    }

    pub(crate) fn queued_duration(&self) -> Duration {
        duration_from_samples(self.queued_samples(), self.device_rate)
    }

    pub(crate) fn headroom_duration(&self) -> Duration {
        duration_from_samples(self.headroom_samples(), self.device_rate)
    }

    pub(crate) fn submitted_samples(&self) -> u64 {
        self.submitted_samples.load(Ordering::Relaxed)
    }

    pub(crate) fn next_output_pts(&self) -> Option<i64> {
        self.producer.next_output_pts()
    }

    pub(crate) fn underrun_callbacks(&self) -> u64 {
        self.underrun_callbacks.load(Ordering::Relaxed)
    }

    pub(crate) fn underrun_samples(&self) -> u64 {
        self.underrun_samples.load(Ordering::Relaxed)
    }

    pub(crate) fn is_playing(&self) -> bool {
        self.stream.is_playing()
    }

    pub(crate) fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    pub(crate) fn device_rate(&self) -> u32 {
        self.device_rate
    }

    pub(crate) fn media_origin(&self) -> Option<Duration> {
        self.media_origin
    }

    pub(crate) fn media_position(&self) -> Option<Duration> {
        let media_origin = self.media_origin?;
        let timeline_origin = self.producer.origin_pts()?;
        let next_output = self.producer.next_output_pts()?;
        let elapsed_samples = next_output.saturating_sub(timeline_origin).max(0) as u64;
        Some(media_origin.saturating_add(duration_from_samples(elapsed_samples, self.device_rate)))
    }
}

fn validate_device_format(
    source_channels: u16,
    device: StreamFormat,
    backend_name: &str,
) -> Result<(), String> {
    if device.channels != source_channels {
        return Err(format!(
            "oxideav-sysaudio {backend_name} negotiated {} channels for a {source_channels}-channel source; channel remixing is not implemented yet",
            device.channels
        ));
    }
    if device.sample_rate == 0 {
        return Err(format!(
            "oxideav-sysaudio {backend_name} negotiated an invalid sample rate"
        ));
    }
    Ok(())
}

fn pcm_block(
    frame: &AudioFrame,
    params: AudioStreamParams,
    pts: Option<i64>,
) -> Result<PcmBlock, String> {
    let channels = decode_to_f32(frame, params.format, params.channels)
        .map_err(|error| format!("convert decoded audio to f32: {error}"))?;
    Ok(PcmBlock {
        pts,
        samples: u64::from(frame.samples),
        interleaved: interleave(&channels, frame.samples as usize)?,
    })
}

fn interleave(channels: &[Vec<f32>], samples: usize) -> Result<Vec<f32>, String> {
    if channels.is_empty() {
        return Err("decoded audio contains zero channels".into());
    }
    if channels.iter().any(|channel| channel.len() != samples) {
        return Err("decoded audio channel sample counts do not match frame.samples".into());
    }
    let mut output = Vec::with_capacity(samples.saturating_mul(channels.len()));
    for sample in 0..samples {
        for channel in channels {
            output.push(channel[sample]);
        }
    }
    Ok(output)
}

fn duration_from_samples(samples: u64, rate: u32) -> Duration {
    if rate == 0 {
        return Duration::ZERO;
    }
    let rate = rate as u64;
    let seconds = samples / rate;
    let nanos = ((samples % rate) * 1_000_000_000 / rate) as u32;
    Duration::new(seconds, nanos)
}

#[cfg(test)]
mod tests {
    use ::oxideav::core::{CodecId, SampleFormat as CoreSampleFormat};

    use super::*;

    fn mock_audio_params() -> CodecParameters {
        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(CoreSampleFormat::F32);
        params
    }

    fn f32_stereo_frame(samples: usize, pts: Option<i64>) -> AudioFrame {
        let mut bytes = Vec::with_capacity(samples * 2 * 4);
        for _ in 0..samples {
            bytes.extend_from_slice(&0.25f32.to_le_bytes());
            bytes.extend_from_slice(&(-0.25f32).to_le_bytes());
        }
        AudioFrame {
            samples: samples as u32,
            pts,
            data: vec![bytes],
        }
    }

    #[test]
    fn interleave_preserves_sample_then_channel_order() {
        let output = interleave(&[vec![1.0, 2.0, 3.0], vec![10.0, 20.0, 30.0]], 3).unwrap();
        assert_eq!(output, vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);
    }

    #[test]
    fn duration_from_samples_is_exact_at_whole_and_fractional_seconds() {
        assert_eq!(
            duration_from_samples(48_000, 48_000),
            Duration::from_secs(1)
        );
        assert_eq!(
            duration_from_samples(12_000, 48_000),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn interleave_rejects_mismatched_channel_lengths() {
        let error = interleave(&[vec![1.0, 2.0], vec![10.0]], 2).unwrap_err();
        assert!(error.contains("sample counts"));
    }

    #[test]
    fn muted_output_sets_software_gain_to_zero() {
        let driver = sysaudio::driver_by_name("mock").expect("mock sysaudio driver");
        let output =
            AudioOutput::open_with_driver(driver, &mock_audio_params(), TimeBase::AUDIO_48K, true)
                .unwrap();
        assert_eq!(output.stream.volume(), 0.0);
    }

    #[test]
    fn audio_frame_pts_is_rescaled_to_integer_device_sample_pts() {
        let driver = sysaudio::driver_by_name("mock").expect("mock sysaudio driver");
        let mut output =
            AudioOutput::open_with_driver(driver, &mock_audio_params(), TimeBase::MPEG_TS, false)
                .unwrap();
        output
            .queue(&f32_stereo_frame(2_400, Some(90_000)))
            .unwrap();
        assert_eq!(output.producer.origin_pts(), Some(48_000));
        assert_eq!(output.producer.ring_end_pts(), Some(50_400));
    }

    #[test]
    fn missing_frame_pts_is_accepted_as_contiguous() {
        let driver = sysaudio::driver_by_name("mock").expect("mock sysaudio driver");
        let mut output =
            AudioOutput::open_with_driver(driver, &mock_audio_params(), TimeBase::AUDIO_48K, false)
                .unwrap();
        output.queue(&f32_stereo_frame(1_000, Some(5_000))).unwrap();
        output.queue(&f32_stereo_frame(500, None)).unwrap();
        assert_eq!(output.producer.ring_end_pts(), Some(6_500));
    }

    #[test]
    fn device_rate_mismatch_is_accepted_for_resampling() {
        let device = StreamFormat {
            sample_rate: 44_100,
            channels: 2,
            format: sysaudio::SampleFormat::F32,
        };
        validate_device_format(2, device, "mock").unwrap();
    }

    #[test]
    fn device_channel_mismatch_is_rejected() {
        let device = StreamFormat {
            sample_rate: 48_000,
            channels: 1,
            format: sysaudio::SampleFormat::F32,
        };
        let error = validate_device_format(2, device, "mock").unwrap_err();
        assert!(error.contains("channel remixing is not implemented"));
    }

    fn resampled_mock_output() -> AudioOutput {
        let driver = sysaudio::driver_by_name("mock").unwrap();
        let mut output =
            AudioOutput::open_with_driver(driver, &mock_audio_params(), TimeBase::MPEG_TS, true)
                .unwrap();
        // The mock accepts requested rates exactly. Supply 44.1 kHz PCM to its
        // 48 kHz device to exercise the same conversion as a negotiating backend.
        output.source_params.sample_rate = 44_100;
        output.resampler = Some(Resample::new(44_100, 48_000).unwrap());
        output
    }

    #[test]
    fn resampled_audio_preserves_duration_across_rounded_frame_timestamps() {
        let mut output = resampled_mock_output();
        for index in 0..100 {
            let pts = 90_000 + index * 1024 * 90_000 / 44_100;
            assert_eq!(
                output.queue(&f32_stereo_frame(1024, Some(pts))).unwrap(),
                QueueResult::Queued
            );
        }
        assert_eq!(output.producer.origin_pts(), Some(48_000));
        let expected = (100_i64 * 1024 * 48_000 + 44_099) / 44_100;
        assert_eq!(output.converted_input_samples, 102_400);
        assert_eq!(output.converted_output_samples, expected);
        assert_eq!(output.producer.ring_end_pts(), Some(48_000 + expected));
        output.finish_input().unwrap();
        let end = output.producer.ring_end_pts();
        assert!(end.unwrap() > 48_000 + expected);
        output.finish_input().unwrap();
        assert_eq!(
            output.producer.ring_end_pts(),
            end,
            "EOF flush is idempotent"
        );
    }

    #[test]
    fn resampling_retries_cached_pcm_and_resets_at_timestamp_discontinuities() {
        let mut output = resampled_mock_output();
        let (producer, mut consumer) = pcm_timeline_ring(1500, 2);
        output.producer = producer;
        let first = f32_stereo_frame(1024, Some(90_000));
        output.queue(&first).unwrap();
        let second = f32_stereo_frame(1024, Some(90_000 + 1024 * 90_000 / 44_100));
        assert_eq!(output.queue(&second).unwrap(), QueueResult::Deferred);
        let samples = output.converted_output_samples;
        assert_eq!(output.queue(&second).unwrap(), QueueResult::Deferred);
        assert_eq!(output.converted_output_samples, samples);
        consumer.fill(&mut vec![0.0; 1115 * 2]);
        assert_eq!(output.queue(&second).unwrap(), QueueResult::Queued);
        assert_eq!(output.converted_input_samples, 2048);
        consumer.fill(&mut vec![0.0; 1115 * 2]);
        let jumped = f32_stereo_frame(1024, Some(180_000));
        assert_eq!(output.queue(&jumped).unwrap(), QueueResult::Deferred);
        assert_eq!(output.conversion_origin, Some(88_200));
        assert_eq!(output.converted_input_samples, 1024);
        assert_eq!(output.pending_pcm.front().unwrap().pts, Some(96_000));
    }

    #[test]
    fn mock_output_advances_next_output_pts_for_every_submitted_block() {
        let driver = sysaudio::driver_by_name("mock").expect("mock sysaudio driver");
        let mut output =
            AudioOutput::open_with_driver(driver, &mock_audio_params(), TimeBase::AUDIO_48K, false)
                .unwrap();
        output.set_media_origin(Duration::from_secs(2)).unwrap();
        output.queue(&f32_stereo_frame(2_400, Some(0))).unwrap();
        assert!(output.preroll_ready());
        assert_eq!(output.media_position(), Some(Duration::from_secs(2)));

        output.set_paused(false).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while output.submitted_samples() < 2_400 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        output.set_paused(true).unwrap();
        assert!(output.submitted_samples() >= 2_400);
        assert!(
            output.media_position().expect("audio timeline initialised")
                >= Duration::from_millis(2_050)
        );
    }
}
