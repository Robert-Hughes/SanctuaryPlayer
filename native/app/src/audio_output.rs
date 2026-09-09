use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ::oxideav::core::{AudioFrame, CodecParameters};
use oxideav_audio_filter::{
    AudioFilter, AudioStreamParams, Resample, sample_convert::decode_to_f32,
};
use oxideav_sysaudio::{self as sysaudio, Driver, StreamRequest};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

const RING_SECONDS: usize = 4;
const PREROLL_MILLIS: u64 = 50;

/// Sanctuary-owned decoded-audio output. OxideAV pushes decoded `AudioFrame`s
/// on the application thread; the platform audio callback pulls interleaved
/// f32 samples from the bounded SPSC ring without taking a mutex.
pub(crate) struct AudioOutput {
    stream: sysaudio::Stream,
    producer: ringbuf::HeapProd<f32>,
    played_samples: Arc<AtomicU64>,
    source_params: AudioStreamParams,
    device_rate: u32,
    device_channels: u16,
    resampler: Option<Resample>,
    media_origin: Option<Duration>,
    preroll_target_samples: u64,
    preroll_done: bool,
    user_paused: bool,
    backend_name: &'static str,
}

impl AudioOutput {
    pub(crate) fn open(params: &CodecParameters) -> Result<Self, String> {
        let driver = sysaudio::default_driver()
            .ok_or_else(|| "oxideav-sysaudio: no usable audio output backend".to_owned())?;
        Self::open_with_driver(driver, params)
    }

    fn open_with_driver(driver: Driver, params: &CodecParameters) -> Result<Self, String> {
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
        let source_params = AudioStreamParams {
            format: source_format,
            channels: source_channels,
            sample_rate: source_rate,
        };

        let capacity = (source_rate.max(48_000) as usize)
            .saturating_mul(source_channels.max(1) as usize)
            .saturating_mul(RING_SECONDS)
            .max(8192);
        let rb = HeapRb::<f32>::new(capacity);
        let (producer, mut consumer) = rb.split();
        let played_samples = Arc::new(AtomicU64::new(0));
        let played_samples_cb = Arc::clone(&played_samples);
        let callback_channels = source_channels.max(1) as usize;

        let request = StreamRequest::new(source_rate, source_channels);
        let mut stream = sysaudio::open(driver, request, move |out, _info| {
            let written = consumer.pop_slice(out);
            out[written..].fill(0.0);
            debug_assert_eq!(written % callback_channels, 0);
            played_samples_cb.fetch_add((written / callback_channels) as u64, Ordering::Relaxed);
        })
        .map_err(|error| format!("oxideav-sysaudio {} open failed: {error}", driver.name()))?;

        // Streams start immediately. Keep the device paused until Sanctuary has
        // enough decoded PCM for a small preroll and the user actually presses
        // Play. The callback can race once before this pause, but the ring is
        // empty so it can emit only silence and the media clock stays at zero.
        stream
            .pause()
            .map_err(|error| format!("oxideav-sysaudio {} pause failed: {error}", driver.name()))?;

        let device = stream.format();
        if device.channels != source_channels {
            return Err(format!(
                "oxideav-sysaudio {} negotiated {} channels for a {}-channel source; channel remixing is not implemented yet",
                driver.name(),
                device.channels,
                source_channels
            ));
        }

        let resampler = if device.sample_rate != source_rate {
            Some(
                Resample::new(source_rate, device.sample_rate)
                    .map_err(|error| format!("configure audio resampler: {error}"))?,
            )
        } else {
            None
        };
        let preroll_target_samples = ((device.sample_rate as u64) * PREROLL_MILLIS / 1000).max(1);

        eprintln!(
            "SanctuaryPlayer: audio output sysaudio/{} source={}Hz {}ch {:?} device={}Hz {}ch {:?} preroll={}ms",
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
            played_samples,
            source_params,
            device_rate: device.sample_rate,
            device_channels: device.channels,
            resampler,
            media_origin: None,
            preroll_target_samples,
            preroll_done: false,
            user_paused: true,
            backend_name: driver.name(),
        })
    }

    pub(crate) fn set_media_origin(&mut self, origin: Duration) -> Result<(), String> {
        if self.media_origin.is_none() {
            self.media_origin = Some(origin);
        }
        self.apply_play_state()
    }

    pub(crate) fn queue(&mut self, frame: &AudioFrame) -> Result<(), String> {
        if self.resampler.is_some() {
            let output = self
                .resampler
                .as_mut()
                .expect("resampler checked above")
                .process(frame, self.source_params)
                .map_err(|error| format!("resample decoded audio: {error}"))?;
            for frame in output {
                self.queue_device_rate_frame(&frame)?;
            }
        } else {
            self.queue_device_rate_frame(frame)?;
        }
        self.maybe_finish_preroll()
    }

    pub(crate) fn finish_input(&mut self) -> Result<(), String> {
        let output = match self.resampler.as_mut() {
            Some(resampler) => resampler
                .flush(self.source_params)
                .map_err(|error| format!("flush audio resampler: {error}"))?,
            None => Vec::new(),
        };
        for frame in output {
            self.queue_device_rate_frame(&frame)?;
        }
        // EOF is also a preroll boundary: a clip shorter than the normal
        // target must still be allowed to start and drain.
        self.preroll_done = true;
        self.apply_play_state()
    }

    fn queue_device_rate_frame(&mut self, frame: &AudioFrame) -> Result<(), String> {
        let channels = decode_to_f32(
            frame,
            self.source_params.format,
            self.source_params.channels,
        )
        .map_err(|error| format!("convert decoded audio to f32: {error}"))?;
        let interleaved = interleave(&channels, frame.samples as usize)?;
        if self.producer.vacant_len() < interleaved.len() {
            return Err(format!(
                "audio ring has insufficient headroom: need {} f32 samples, have {}",
                interleaved.len(),
                self.producer.vacant_len()
            ));
        }
        let pushed = self.producer.push_slice(&interleaved);
        if pushed != interleaved.len() {
            return Err(format!(
                "audio ring accepted only {pushed} of {} f32 samples",
                interleaved.len()
            ));
        }
        Ok(())
    }

    fn maybe_finish_preroll(&mut self) -> Result<(), String> {
        if !self.preroll_done && self.queued_samples() >= self.preroll_target_samples {
            self.preroll_done = true;
            self.apply_play_state()?;
        }
        Ok(())
    }

    pub(crate) fn set_paused(&mut self, paused: bool) -> Result<(), String> {
        self.user_paused = paused;
        self.apply_play_state()
    }

    fn apply_play_state(&mut self) -> Result<(), String> {
        let should_play = self.preroll_done && self.media_origin.is_some() && !self.user_paused;
        if should_play == self.stream.is_playing() {
            return Ok(());
        }
        let result = if should_play {
            self.stream.play()
        } else {
            self.stream.pause()
        };
        result.map_err(|error| {
            format!(
                "oxideav-sysaudio {} {} failed: {error}",
                self.backend_name,
                if should_play { "play" } else { "pause" }
            )
        })
    }

    pub(crate) fn preroll_ready(&self) -> bool {
        self.preroll_done
    }

    pub(crate) fn queued_samples(&self) -> u64 {
        (self.producer.occupied_len() / self.device_channels.max(1) as usize) as u64
    }

    pub(crate) fn headroom_samples(&self) -> u64 {
        (self.producer.vacant_len() / self.device_channels.max(1) as usize) as u64
    }

    pub(crate) fn headroom_floor_samples(&self) -> u64 {
        (self.device_rate as u64 / 4).max(1)
    }

    pub(crate) fn media_position(&self) -> Option<Duration> {
        let origin = self.media_origin?;
        Some(origin.saturating_add(duration_from_samples(
            self.played_samples.load(Ordering::Relaxed),
            self.device_rate,
        )))
    }
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
    use ::oxideav::core::{CodecId, SampleFormat};

    use super::*;

    fn mock_audio_params() -> CodecParameters {
        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::F32);
        params
    }

    fn f32_stereo_frame(samples: usize) -> AudioFrame {
        let mut bytes = Vec::with_capacity(samples * 2 * 4);
        for _ in 0..samples {
            bytes.extend_from_slice(&0.25f32.to_le_bytes());
            bytes.extend_from_slice(&(-0.25f32).to_le_bytes());
        }
        AudioFrame {
            samples: samples as u32,
            pts: Some(0),
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
    fn mock_output_uses_real_pcm_consumption_as_master_clock() {
        let driver = sysaudio::driver_by_name("mock").expect("mock sysaudio driver");
        let mut output = AudioOutput::open_with_driver(driver, &mock_audio_params()).unwrap();
        output.set_media_origin(Duration::from_secs(2)).unwrap();

        output.queue(&f32_stereo_frame(2_400)).unwrap();
        assert!(output.preroll_ready());
        assert_eq!(output.media_position(), Some(Duration::from_secs(2)));

        output.set_paused(false).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while output.queued_samples() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(output.queued_samples(), 0, "mock device did not drain PCM");
        let drained = output.media_position().expect("audio clock anchored");
        assert!(drained >= Duration::from_millis(2_045));
        assert!(drained <= Duration::from_millis(2_055));

        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(output.media_position(), Some(drained));
        output.set_paused(true).unwrap();
    }

    #[test]
    fn source_format_is_kept_for_resampler_conversion() {
        let params = AudioStreamParams {
            format: SampleFormat::F32P,
            channels: 2,
            sample_rate: 48_000,
        };
        assert_eq!(params.format, SampleFormat::F32P);
    }
}
