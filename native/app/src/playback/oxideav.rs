use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use ::oxideav::core::{Error, Frame, FrameLease, MediaType, Packet, StreamInfo, TimeBase};
use ::oxideav::pipeline::{CodecPreferences, Executor, ExecutorHandle, Job, JobSink};
use serde_json::json;
use url::Url;

use crate::audio_output::AudioOutput;
use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

use super::{DecodeMode, PlaybackBackend};

const SESSION_CHANNEL_CAP: usize = 2;
const VIDEO_QUEUE_TARGET: usize = 4;
const VIDEO_QUEUE_MAX: usize = 8;
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(1);

pub struct OxidePlayback {
    source: VideoSource,
    state: PlaybackState,
    position: Duration,
    duration: Option<Duration>,
    rate: f32,
    rates: Vec<f32>,
    qualities: Vec<Quality>,
    rx: Receiver<SessionMsg>,
    executor: Option<ExecutorHandle>,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    audio_output: Option<AudioOutput>,
    video_queue: VecDeque<FrameLease>,
    timeline_origin_seconds: Option<f64>,
    first_video_seconds: Option<f64>,
    first_audio_seconds: Option<f64>,
    first_frame_presented: bool,
    sink_finished: bool,
    diagnostics: PlaybackDiagnostics,
}

struct PlaybackDiagnostics {
    last_status: Instant,
    received_audio_frames: u64,
    received_video_frames: u64,
    presented_video_frames: u64,
    dropped_video_frames: u64,
}

impl PlaybackDiagnostics {
    fn new() -> Self {
        Self {
            last_status: Instant::now(),
            received_audio_frames: 0,
            received_video_frames: 0,
            presented_video_frames: 0,
            dropped_video_frames: 0,
        }
    }
}

enum SessionMsg {
    Started(Vec<StreamInfo>),
    Frame { kind: MediaType, frame: FrameLease },
    Finished,
}

impl SessionMsg {
    fn label(&self) -> &'static str {
        match self {
            Self::Started(_) => "start",
            Self::Frame {
                kind: MediaType::Audio,
                ..
            } => "audio",
            Self::Frame {
                kind: MediaType::Video,
                ..
            } => "video",
            Self::Frame { .. } => "other",
            Self::Finished => "finish",
        }
    }
}

struct SessionSink {
    tx: SyncSender<SessionMsg>,
    blocked_sends: u64,
    last_backpressure_log: Option<Instant>,
}

impl SessionSink {
    fn new(tx: SyncSender<SessionMsg>) -> Self {
        Self {
            tx,
            blocked_sends: 0,
            last_backpressure_log: None,
        }
    }

    fn send(&mut self, message: SessionMsg) -> ::oxideav::core::Result<()> {
        let label = message.label();
        match self.tx.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(message)) => {
                self.blocked_sends = self.blocked_sends.saturating_add(1);
                let now = Instant::now();
                if self
                    .last_backpressure_log
                    .is_none_or(|last| now.duration_since(last) >= DIAGNOSTIC_INTERVAL)
                {
                    eprintln!(
                        "SanctuaryPlayer: pipeline sink backpressure channel=full waiting_for={} blocked_sends={}",
                        label, self.blocked_sends
                    );
                    self.last_backpressure_log = Some(now);
                }
                self.tx
                    .send(message)
                    .map_err(|_| Error::other("SanctuaryPlayer: playback receiver dropped"))
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(Error::other("SanctuaryPlayer: playback receiver dropped"))
            }
        }
    }
}

impl JobSink for SessionSink {
    fn start(&mut self, streams: &[StreamInfo]) -> ::oxideav::core::Result<()> {
        self.send(SessionMsg::Started(streams.to_vec()))
    }

    fn write_packet(&mut self, _kind: MediaType, _packet: &Packet) -> ::oxideav::core::Result<()> {
        Err(Error::unsupported(
            "SanctuaryPlayer playback sink requires decoded audio/video frames",
        ))
    }

    fn write_frame(&mut self, kind: MediaType, frame: &Frame) -> ::oxideav::core::Result<()> {
        self.write_frame_lease(kind, FrameLease::from_frame(frame.clone()))
    }

    fn write_frame_lease(
        &mut self,
        kind: MediaType,
        frame: FrameLease,
    ) -> ::oxideav::core::Result<()> {
        if !matches!(kind, MediaType::Audio | MediaType::Video) {
            return Ok(());
        }
        self.send(SessionMsg::Frame { kind, frame })
    }

    fn finish(&mut self) -> ::oxideav::core::Result<()> {
        self.send(SessionMsg::Finished)
    }
}

impl OxidePlayback {
    pub fn open(
        source: VideoSource,
        m3u8_url: Url,
        decode_mode: DecodeMode,
    ) -> Result<Self, String> {
        let input = format!("hls+{}", m3u8_url.as_str());
        let job_json = serde_json::to_string(&json!({
            "@in": { "all": [{ "from": input }] },
            "@display": {
                "audio": [{ "from": "@in" }],
                "video": [{ "from": "@in" }]
            },
        }))
        .map_err(|error| format!("build OxideAV playback job: {error}"))?;
        let job = Job::from_json(&job_json).map_err(|error| error.to_string())?;
        job.validate().map_err(|error| error.to_string())?;

        let mut registries = ::oxideav::Registries::new();
        oxideav_meta::register_all(&mut registries);

        let codec_preferences = codec_preferences(decode_mode);
        let (tx, rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        let sink = Box::new(SessionSink::new(tx));
        let executor = Executor::new(&job, &registries)
            .with_sink_override("@display", sink)
            .with_codec_preferences(codec_preferences)
            .with_threads(0)
            .spawn()
            .map_err(|error| format!("start OxideAV playback: {error}"))?;

        let streams = match rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(SessionMsg::Started(streams)) => streams,
            Ok(_) => {
                stop_executor(executor);
                return Err("OxideAV emitted media before stream initialisation".into());
            }
            Err(error) => {
                stop_executor(executor);
                return Err(format!("waiting for OxideAV stream information: {error}"));
            }
        };
        let video_stream = streams
            .iter()
            .find(|stream| stream.params.media_type == MediaType::Video)
            .cloned()
            .ok_or_else(|| "OxideAV source contains no video stream".to_owned())?;
        let audio_stream = streams
            .iter()
            .find(|stream| stream.params.media_type == MediaType::Audio)
            .cloned();

        let audio_output = match audio_stream.as_ref() {
            Some(stream) => match AudioOutput::open(&stream.params) {
                Ok(output) => Some(output),
                Err(error) => {
                    stop_executor(executor);
                    return Err(format!("open audio output: {error}"));
                }
            },
            None => None,
        };
        let duration = streams.iter().filter_map(stream_duration).max();
        let first_video_seconds = stream_start_seconds(&video_stream);
        let first_audio_seconds = audio_stream.as_ref().and_then(stream_start_seconds);
        let timeline_origin_seconds = match (
            first_video_seconds,
            first_audio_seconds,
            audio_stream.is_some(),
        ) {
            (Some(video), Some(audio), true) => Some(video.min(audio)),
            (Some(video), _, false) => Some(video),
            _ => None,
        };

        eprintln!(
            "SanctuaryPlayer: OxideAV video stream mode={} codec={} {}x{} time_base={}/{}",
            decode_mode,
            video_stream.params.codec_id,
            video_stream.params.width.unwrap_or(0),
            video_stream.params.height.unwrap_or(0),
            video_stream.time_base.num(),
            video_stream.time_base.den(),
        );
        if let Some(stream) = audio_stream.as_ref() {
            eprintln!(
                "SanctuaryPlayer: OxideAV audio stream codec={} rate={}Hz channels={} format={:?} time_base={}/{}",
                stream.params.codec_id,
                stream.params.sample_rate.unwrap_or(0),
                stream.params.resolved_channels().unwrap_or(0),
                stream.params.sample_format,
                stream.time_base.num(),
                stream.time_base.den(),
            );
        }

        let rates = if audio_output.is_some() {
            // Audio is the master clock. Pitch-preserving time stretch is a
            // separate milestone, so real A/V playback is intentionally 1x.
            vec![1.0]
        } else {
            vec![0.25, 0.5, 1.0, 1.5, 2.0]
        };

        Ok(Self {
            source,
            state: PlaybackState::Paused,
            position: Duration::ZERO,
            duration,
            rate: 1.0,
            rates,
            qualities: vec![Quality::new("hls-auto", "HLS (up to 720p)")],
            rx,
            executor: Some(executor),
            video_stream,
            audio_stream,
            audio_output,
            video_queue: VecDeque::new(),
            timeline_origin_seconds,
            first_video_seconds,
            first_audio_seconds,
            first_frame_presented: false,
            sink_finished: false,
            diagnostics: PlaybackDiagnostics::new(),
        })
    }

    fn pump_block_reason(&self) -> Option<&'static str> {
        if self.sink_finished {
            return Some("sink-finished");
        }
        if matches!(self.state, PlaybackState::Error(_)) {
            return Some("playback-error");
        }

        if let Some(audio) = self.audio_output.as_ref() {
            return av_pump_block_reason(AvPumpState {
                state: &self.state,
                first_frame_presented: self.first_frame_presented,
                video_queue_len: self.video_queue.len(),
                audio_preroll_ready: audio.preroll_ready(),
                audio_queued_samples: audio.queued_samples(),
                audio_target_samples: audio.queue_target_samples(),
                audio_headroom_samples: audio.headroom_samples(),
                audio_minimum_headroom_samples: audio.minimum_headroom_samples(),
            });
        }

        let target = if matches!(self.state, PlaybackState::Paused) {
            usize::from(self.video_queue.is_empty())
        } else {
            VIDEO_QUEUE_TARGET
        };
        (self.video_queue.len() >= target).then_some("video-buffer-ready")
    }

    fn should_pump(&self) -> bool {
        self.pump_block_reason().is_none()
    }

    fn pump_session(&mut self) {
        while self.should_pump() {
            match self.rx.try_recv() {
                Ok(message) => {
                    if let Err(error) = self.handle_session_message(message) {
                        self.fail(error);
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sink_finished = true;
                    break;
                }
            }
        }

        self.collect_executor_result();
        self.update_end_state();
    }

    fn handle_session_message(&mut self, message: SessionMsg) -> Result<(), String> {
        match message {
            SessionMsg::Started(_) => Ok(()),
            SessionMsg::Frame { kind, frame } => {
                self.observe_frame_timestamp(kind, frame.pts());
                self.sync_audio_origin()?;
                match kind {
                    MediaType::Video => {
                        self.diagnostics.received_video_frames =
                            self.diagnostics.received_video_frames.saturating_add(1);
                        if self.video_queue.len() >= VIDEO_QUEUE_MAX {
                            self.diagnostics.dropped_video_frames =
                                self.diagnostics.dropped_video_frames.saturating_add(1);
                            if self.diagnostics.dropped_video_frames <= 3
                                || self.diagnostics.dropped_video_frames.is_multiple_of(60)
                            {
                                eprintln!(
                                    "SanctuaryPlayer: video queue hard cap reached; dropping decoded frame queue={} dropped={}",
                                    self.video_queue.len(),
                                    self.diagnostics.dropped_video_frames
                                );
                            }
                            return Ok(());
                        }
                        self.video_queue.push_back(frame);
                        Ok(())
                    }
                    MediaType::Audio => {
                        self.diagnostics.received_audio_frames =
                            self.diagnostics.received_audio_frames.saturating_add(1);
                        let audio = self.audio_output.as_mut().ok_or_else(|| {
                            "OxideAV produced audio without an audio output".to_owned()
                        })?;
                        let audio_frame = match frame.as_frame() {
                            Some(Frame::Audio(audio_frame)) => audio_frame,
                            _ => {
                                return Err(
                                    "OxideAV audio output was not an owned AudioFrame".to_owned()
                                );
                            }
                        };
                        audio.queue(audio_frame)
                    }
                    _ => Ok(()),
                }
            }
            SessionMsg::Finished => {
                if let Some(audio) = self.audio_output.as_mut() {
                    audio.finish_input()?;
                }
                self.sink_finished = true;
                Ok(())
            }
        }
    }

    fn observe_frame_timestamp(&mut self, kind: MediaType, pts: Option<i64>) {
        let Some(pts) = pts else {
            return;
        };
        let stream = match kind {
            MediaType::Video => Some(&self.video_stream),
            MediaType::Audio => self.audio_stream.as_ref(),
            _ => None,
        };
        let Some(stream) = stream else {
            return;
        };
        let seconds = stream.time_base.seconds_of(pts);
        if !seconds.is_finite() {
            return;
        }
        match kind {
            MediaType::Video if self.first_video_seconds.is_none() => {
                self.first_video_seconds = Some(seconds);
            }
            MediaType::Audio if self.first_audio_seconds.is_none() => {
                self.first_audio_seconds = Some(seconds);
            }
            _ => {}
        }
        if self.timeline_origin_seconds.is_none() {
            self.timeline_origin_seconds = match (
                self.first_video_seconds,
                self.first_audio_seconds,
                self.audio_stream.is_some(),
            ) {
                (Some(video), Some(audio), true) => Some(video.min(audio)),
                (Some(video), _, false) => Some(video),
                _ => None,
            };
        }
    }

    fn sync_audio_origin(&mut self) -> Result<(), String> {
        let (Some(origin), Some(first_audio), Some(audio)) = (
            self.timeline_origin_seconds,
            self.first_audio_seconds,
            self.audio_output.as_mut(),
        ) else {
            return Ok(());
        };
        let relative = (first_audio - origin).max(0.0);
        audio.set_media_origin(Duration::from_secs_f64(relative))
    }

    fn frame_position_for_kind(&self, kind: MediaType, pts: Option<i64>) -> Option<Duration> {
        let stream = match kind {
            MediaType::Video => Some(&self.video_stream),
            MediaType::Audio => self.audio_stream.as_ref(),
            _ => None,
        }?;
        relative_stream_position(stream, pts?, self.timeline_origin_seconds?)
    }

    fn collect_executor_result(&mut self) {
        let finished = self
            .executor
            .as_ref()
            .is_some_and(ExecutorHandle::has_finished);
        if !finished {
            return;
        }
        let Some(executor) = self.executor.take() else {
            return;
        };
        if let Err(error) = executor.stop() {
            self.fail(format!("OxideAV playback failed: {error}"));
        }
    }

    fn fail(&mut self, message: String) {
        eprintln!("SanctuaryPlayer: {message}");
        if let Some(executor) = self.executor.as_ref() {
            executor.request_abort();
        }
        if let Some(audio) = self.audio_output.as_mut() {
            let _ = audio.set_paused(true);
        }
        self.state = PlaybackState::Error(message);
    }

    fn update_end_state(&mut self) {
        if !self.sink_finished || !self.video_queue.is_empty() {
            return;
        }
        if self
            .audio_output
            .as_ref()
            .is_some_and(|audio| audio.queued_samples() != 0)
        {
            return;
        }
        if matches!(self.state, PlaybackState::Error(_)) {
            return;
        }
        if self.first_frame_presented {
            self.state = PlaybackState::Ended;
        } else if self.executor.is_none() {
            self.state = PlaybackState::Error(
                "OxideAV playback finished without producing a video frame".into(),
            );
        }
    }

    fn frame_position(&self, frame: &FrameLease) -> Option<Duration> {
        self.frame_position_for_kind(MediaType::Video, frame.pts())
    }

    fn take_due_frame(&mut self) -> Option<FrameLease> {
        self.pump_session();

        if !self.first_frame_presented {
            let frame = self.video_queue.pop_front()?;
            self.first_frame_presented = true;
            self.diagnostics.presented_video_frames =
                self.diagnostics.presented_video_frames.saturating_add(1);
            return Some(frame);
        }
        if !matches!(self.state, PlaybackState::Playing) {
            return None;
        }

        let mut due = None;
        let mut due_count = 0_u64;
        while let Some(next) = self.video_queue.front() {
            let is_due = self
                .frame_position(next)
                .is_none_or(|frame_position| frame_position <= self.position);
            if !is_due {
                break;
            }
            due = self.video_queue.pop_front();
            due_count = due_count.saturating_add(1);
        }
        if due.is_some() {
            self.diagnostics.presented_video_frames =
                self.diagnostics.presented_video_frames.saturating_add(1);
            self.diagnostics.dropped_video_frames = self
                .diagnostics
                .dropped_video_frames
                .saturating_add(due_count.saturating_sub(1));
        }
        due
    }

    fn update_position(&mut self, elapsed: Duration) {
        if let Some(audio) = self.audio_output.as_ref() {
            if let Some(position) = audio.media_position() {
                self.position = position;
            }
        } else if matches!(self.state, PlaybackState::Playing) && !self.video_queue.is_empty() {
            self.position = self.position.saturating_add(elapsed.mul_f32(self.rate));
        }

        if let Some(duration) = self.duration
            && self.position >= duration
        {
            self.position = duration;
        }
    }

    fn maybe_log_status(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.diagnostics.last_status) < DIAGNOSTIC_INTERVAL {
            return;
        }
        self.diagnostics.last_status = now;

        let front = self
            .video_queue
            .front()
            .and_then(|frame| self.frame_position(frame))
            .map(|position| format!("{:.3}", position.as_secs_f64()))
            .unwrap_or_else(|| "-".into());
        let back = self
            .video_queue
            .back()
            .and_then(|frame| self.frame_position(frame))
            .map(|position| format!("{:.3}", position.as_secs_f64()))
            .unwrap_or_else(|| "-".into());
        let pump = self.pump_block_reason().unwrap_or("draining");
        let executor_finished = self
            .executor
            .as_ref()
            .is_none_or(ExecutorHandle::has_finished);

        if let Some(audio) = self.audio_output.as_ref() {
            eprintln!(
                "SanctuaryPlayer: A/V status state={:?} clock={:.3}s pump={} executor_finished={} sink_finished={} video[q={} front={}s back={}s recv={} present={} drop={}] audio[playing={} preroll={} queued={:.1}ms headroom={:.1}ms played_samples={} underrun_callbacks={} underrun_samples={}]",
                self.state,
                self.position.as_secs_f64(),
                pump,
                executor_finished,
                self.sink_finished,
                self.video_queue.len(),
                front,
                back,
                self.diagnostics.received_video_frames,
                self.diagnostics.presented_video_frames,
                self.diagnostics.dropped_video_frames,
                audio.is_playing(),
                audio.preroll_ready(),
                audio.queued_duration().as_secs_f64() * 1000.0,
                audio.headroom_duration().as_secs_f64() * 1000.0,
                audio.played_samples(),
                audio.underrun_callbacks(),
                audio.underrun_samples(),
            );
        } else {
            eprintln!(
                "SanctuaryPlayer: video status state={:?} clock={:.3}s pump={} executor_finished={} sink_finished={} video[q={} front={}s back={}s recv={} present={} drop={}]",
                self.state,
                self.position.as_secs_f64(),
                pump,
                executor_finished,
                self.sink_finished,
                self.video_queue.len(),
                front,
                back,
                self.diagnostics.received_video_frames,
                self.diagnostics.presented_video_frames,
                self.diagnostics.dropped_video_frames,
            );
        }
    }
}

impl PlaybackBackend for OxidePlayback {
    fn open(&mut self, _source: &VideoSource) -> Result<(), String> {
        Err("OxidePlayback requires a resolved HLS URL".into())
    }

    fn source(&self) -> Option<&VideoSource> {
        Some(&self.source)
    }

    fn state(&self) -> &PlaybackState {
        &self.state
    }

    fn play(&mut self) {
        if !matches!(self.state, PlaybackState::Paused) {
            return;
        }
        eprintln!("SanctuaryPlayer: playback -> Playing");
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(false)
        {
            self.fail(error);
            return;
        }
        self.state = PlaybackState::Playing;
    }

    fn pause(&mut self) {
        if !matches!(self.state, PlaybackState::Playing) {
            return;
        }
        eprintln!("SanctuaryPlayer: playback -> Paused");
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(true)
        {
            self.fail(error);
            return;
        }
        self.state = PlaybackState::Paused;
    }

    fn position(&self) -> Duration {
        self.position
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn seek(&mut self, _position: Duration) {
        // HLS media-relative seek integration is a separate milestone. Do not
        // fake a seek by moving only Sanctuary's clock away from the decoder.
    }

    fn available_rates(&self) -> &[f32] {
        &self.rates
    }

    fn playback_rate(&self) -> f32 {
        self.rate
    }

    fn set_playback_rate(&mut self, rate: f32) {
        if self
            .rates
            .iter()
            .any(|candidate| (*candidate - rate).abs() < f32::EPSILON)
        {
            self.rate = rate;
        }
    }

    fn available_qualities(&self) -> &[Quality] {
        &self.qualities
    }

    fn quality(&self) -> Option<&Quality> {
        self.qualities.first()
    }

    fn set_quality(&mut self, _quality_id: &str) {
        // OxideAV's HLS source currently selects one rendition at open.
    }

    fn update(&mut self, elapsed: Duration) {
        self.pump_session();
        self.update_position(elapsed);
        self.update_end_state();
        self.maybe_log_status();
    }

    fn needs_animation(&self) -> bool {
        !matches!(self.state, PlaybackState::Error(_) | PlaybackState::Ended)
            && (matches!(self.state, PlaybackState::Playing | PlaybackState::Seeking)
                || !self.first_frame_presented)
    }

    fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        self.take_due_frame()
    }
}

impl Drop for OxidePlayback {
    fn drop(&mut self) {
        if let Some(audio) = self.audio_output.as_mut() {
            let _ = audio.set_paused(true);
        }
        if let Some(executor) = self.executor.take() {
            executor.request_abort();
            drop(executor);
        }
    }
}

fn codec_preferences(decode_mode: DecodeMode) -> CodecPreferences {
    match decode_mode {
        DecodeMode::Cpu => CodecPreferences {
            no_hardware: true,
            ..Default::default()
        },
        DecodeMode::VdpauReadback | DecodeMode::VdpauDirect => CodecPreferences {
            prefer: vec!["h264_vdpau".into()],
            // Keep the VDPAU contract strict without requiring *audio* codecs
            // to be hardware accelerated as well.
            exclude: vec!["h264_sw".into()],
            boost: 100,
            ..Default::default()
        },
    }
}

struct AvPumpState<'a> {
    state: &'a PlaybackState,
    first_frame_presented: bool,
    video_queue_len: usize,
    audio_preroll_ready: bool,
    audio_queued_samples: u64,
    audio_target_samples: u64,
    audio_headroom_samples: u64,
    audio_minimum_headroom_samples: u64,
}

fn av_pump_block_reason(input: AvPumpState<'_>) -> Option<&'static str> {
    if input.audio_headroom_samples < input.audio_minimum_headroom_samples {
        return Some("audio-ring-near-full");
    }
    if !input.audio_preroll_ready {
        return None;
    }
    if matches!(input.state, PlaybackState::Paused) {
        return (input.first_frame_presented || input.video_queue_len != 0)
            .then_some("paused-buffered");
    }

    let video_ready = input.video_queue_len >= VIDEO_QUEUE_TARGET;
    let audio_ready = input.audio_queued_samples >= input.audio_target_samples;
    (video_ready && audio_ready).then_some("av-buffers-ready")
}

fn stop_executor(executor: ExecutorHandle) {
    executor.request_abort();
    let _ = executor.stop();
}

fn stream_duration(stream: &StreamInfo) -> Option<Duration> {
    let ticks = stream.duration?;
    duration_from_ticks(stream.time_base, ticks)
}

fn stream_start_seconds(stream: &StreamInfo) -> Option<f64> {
    let ticks = stream.start_time?;
    let seconds = stream.time_base.seconds_of(ticks);
    seconds.is_finite().then_some(seconds)
}

fn relative_stream_position(
    stream: &StreamInfo,
    pts: i64,
    origin_seconds: f64,
) -> Option<Duration> {
    let seconds = stream.time_base.seconds_of(pts) - origin_seconds;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn duration_from_ticks(time_base: TimeBase, ticks: i64) -> Option<Duration> {
    let seconds = time_base.seconds_of(ticks);
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use ::oxideav::core::{CodecId, CodecParameters, VideoFrame};

    use super::*;

    fn clock_test_playback() -> (OxidePlayback, SyncSender<SessionMsg>) {
        let (tx, rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        let video_stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(0),
            params: CodecParameters::video(CodecId::new("h264")),
        };
        (
            OxidePlayback {
                source: VideoSource::parse("2386400830").unwrap(),
                state: PlaybackState::Playing,
                position: Duration::from_secs(1),
                duration: None,
                rate: 1.0,
                rates: vec![1.0],
                qualities: Vec::new(),
                rx,
                executor: None,
                video_stream,
                audio_stream: None,
                audio_output: None,
                video_queue: VecDeque::new(),
                timeline_origin_seconds: Some(0.0),
                first_video_seconds: Some(0.0),
                first_audio_seconds: None,
                first_frame_presented: true,
                sink_finished: false,
                diagnostics: PlaybackDiagnostics::new(),
            },
            tx,
        )
    }

    #[test]
    fn playback_clock_stalls_when_decoded_video_queue_is_empty() {
        let (mut playback, tx) = clock_test_playback();

        playback.update(Duration::from_millis(250));
        assert_eq!(playback.position(), Duration::from_secs(1));

        tx.send(SessionMsg::Frame {
            kind: MediaType::Video,
            frame: FrameLease::from_frame(Frame::Video(VideoFrame {
                pts: Some(90_000),
                planes: Vec::new(),
            })),
        })
        .unwrap();
        playback.update(Duration::from_millis(250));
        assert_eq!(playback.position(), Duration::from_millis(1_250));

        assert!(playback.take_due_frame().is_some());
        playback.update(Duration::from_millis(250));
        assert_eq!(playback.position(), Duration::from_millis(1_250));
    }

    #[test]
    fn vdpau_selection_stays_strict_without_requiring_hardware_audio() {
        let prefs = codec_preferences(DecodeMode::VdpauDirect);
        assert!(prefs.prefer.iter().any(|name| name == "h264_vdpau"));
        assert!(prefs.exclude.iter().any(|name| name == "h264_sw"));
        assert!(!prefs.require_hardware);
    }

    #[test]
    fn relative_position_preserves_cross_stream_timestamp_offset() {
        let stream = StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(48_000),
            params: CodecParameters::audio(CodecId::new("aac")),
        };
        assert_eq!(
            relative_stream_position(&stream, 60_000, 1.0),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn av_pump_keeps_draining_when_video_is_ready_but_audio_is_low() {
        assert_eq!(
            av_pump_block_reason(AvPumpState {
                state: &PlaybackState::Playing,
                first_frame_presented: true,
                video_queue_len: VIDEO_QUEUE_TARGET,
                audio_preroll_ready: true,
                audio_queued_samples: 2_400,
                audio_target_samples: 24_000,
                audio_headroom_samples: 180_000,
                audio_minimum_headroom_samples: 4_800,
            }),
            None,
            "a full video target must not block audio messages in the shared session channel"
        );
    }

    #[test]
    fn av_pump_stops_only_after_both_forward_targets_are_ready() {
        assert_eq!(
            av_pump_block_reason(AvPumpState {
                state: &PlaybackState::Playing,
                first_frame_presented: true,
                video_queue_len: VIDEO_QUEUE_TARGET,
                audio_preroll_ready: true,
                audio_queued_samples: 24_000,
                audio_target_samples: 24_000,
                audio_headroom_samples: 168_000,
                audio_minimum_headroom_samples: 4_800,
            }),
            Some("av-buffers-ready")
        );
    }

    #[test]
    fn converts_stream_ticks_to_duration() {
        assert_eq!(
            duration_from_ticks(TimeBase::new(1, 90_000), 180_000),
            Some(Duration::from_secs(2))
        );
    }
}
