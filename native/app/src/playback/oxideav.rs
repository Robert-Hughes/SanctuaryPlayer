use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::{Duration, Instant};

use ::oxideav::core::{Error, Frame, FrameLease, MediaType, Packet, StreamInfo, TimeBase};
use ::oxideav::pipeline::{CodecPreferences, Executor, ExecutorHandle, Job, JobSink};
use oxideav_hls::{HlsPlaylistInfo, HlsVariant};
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
    quality_urls: Vec<Url>,
    quality_index: usize,
    active_quality_index: usize,
    decode_mode: DecodeMode,
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

struct PlaybackSession {
    rx: Receiver<SessionMsg>,
    executor: Option<ExecutorHandle>,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    audio_output: Option<AudioOutput>,
    duration: Option<Duration>,
    rates: Vec<f32>,
    timeline_origin_seconds: Option<f64>,
    first_video_seconds: Option<f64>,
    first_audio_seconds: Option<f64>,
}

fn open_variant_session(
    variant_url: &Url,
    decode_mode: DecodeMode,
) -> Result<PlaybackSession, String> {
    let input = hls_uri(variant_url);
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
    let Some(video_stream) = streams
        .iter()
        .find(|stream| stream.params.media_type == MediaType::Video)
        .cloned()
    else {
        stop_executor(executor);
        return Err("OxideAV source contains no video stream".into());
    };
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
        vec![1.0]
    } else {
        vec![0.25, 0.5, 1.0, 1.5, 2.0]
    };

    Ok(PlaybackSession {
        rx,
        executor: Some(executor),
        video_stream,
        audio_stream,
        audio_output,
        duration,
        rates,
        timeline_origin_seconds,
        first_video_seconds,
        first_audio_seconds,
    })
}

impl OxidePlayback {
    pub fn open(
        source: VideoSource,
        m3u8_url: Url,
        decode_mode: DecodeMode,
    ) -> Result<Self, String> {
        let quality_set = inspect_hls_qualities(&m3u8_url)?;
        let selected_url = quality_set.urls[quality_set.preferred_index].clone();
        eprintln!(
            "SanctuaryPlayer: HLS initial quality={} variant={}",
            quality_set.qualities[quality_set.preferred_index].label, selected_url
        );
        let session = open_variant_session(&selected_url, decode_mode)?;

        Ok(Self {
            source,
            state: PlaybackState::Paused,
            position: Duration::ZERO,
            duration: session.duration,
            rate: 1.0,
            rates: session.rates,
            qualities: quality_set.qualities,
            quality_urls: quality_set.urls,
            quality_index: quality_set.preferred_index,
            active_quality_index: quality_set.preferred_index,
            decode_mode,
            rx: session.rx,
            executor: session.executor,
            video_stream: session.video_stream,
            audio_stream: session.audio_stream,
            audio_output: session.audio_output,
            video_queue: VecDeque::new(),
            timeline_origin_seconds: session.timeline_origin_seconds,
            first_video_seconds: session.first_video_seconds,
            first_audio_seconds: session.first_audio_seconds,
            first_frame_presented: false,
            sink_finished: false,
            diagnostics: PlaybackDiagnostics::new(),
        })
    }

    fn tear_down_session(&mut self) {
        if let Some(audio) = self.audio_output.as_mut() {
            let _ = audio.set_paused(true);
        }

        // A sink worker may be blocked in SyncSender::send(). Dropping its
        // receiver first wakes that send with Disconnected so executor.stop()
        // cannot deadlock waiting for a worker that the UI thread itself has
        // stopped draining.
        let (_placeholder_tx, placeholder_rx) = mpsc::sync_channel(1);
        let old_rx = std::mem::replace(&mut self.rx, placeholder_rx);
        drop(old_rx);

        if let Some(executor) = self.executor.take() {
            stop_executor(executor);
        }
        self.audio_output = None;
        self.video_queue.clear();
    }

    fn install_session(&mut self, session: PlaybackSession) {
        self.rx = session.rx;
        self.executor = session.executor;
        self.video_stream = session.video_stream;
        self.audio_stream = session.audio_stream;
        self.audio_output = session.audio_output;
        self.duration = session.duration;
        self.rates = session.rates;
        self.position = Duration::ZERO;
        self.timeline_origin_seconds = session.timeline_origin_seconds;
        self.first_video_seconds = session.first_video_seconds;
        self.first_audio_seconds = session.first_audio_seconds;
        self.first_frame_presented = false;
        self.sink_finished = false;
        self.video_queue.clear();
        self.diagnostics = PlaybackDiagnostics::new();
    }

    fn switch_quality_with<F>(&mut self, index: usize, opener: F) -> Result<(), String>
    where
        F: FnOnce(&Url, DecodeMode) -> Result<PlaybackSession, String>,
    {
        if index >= self.qualities.len() || index >= self.quality_urls.len() {
            return Err(format!("quality index {index} is out of range"));
        }
        if index == self.active_quality_index {
            self.quality_index = index;
            return Ok(());
        }

        let resume_playing = matches!(self.state, PlaybackState::Playing);
        let old_quality = self.qualities[self.active_quality_index].label.clone();
        let new_quality = self.qualities[index].label.clone();
        let new_url = self.quality_urls[index].clone();
        eprintln!(
            "SanctuaryPlayer: quality switch begin old={} new={} variant={} resume_playing={}",
            old_quality, new_quality, new_url, resume_playing
        );

        self.tear_down_session();
        self.state = PlaybackState::Paused;
        self.position = Duration::ZERO;

        let session = opener(&new_url, self.decode_mode)
            .map_err(|error| format!("switch HLS quality to {new_quality}: {error}"))?;
        self.install_session(session);
        self.quality_index = index;
        self.active_quality_index = index;

        if resume_playing {
            self.play();
        }
        eprintln!(
            "SanctuaryPlayer: quality switch complete active={} variant={} position=0s",
            new_quality, new_url
        );
        Ok(())
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
        self.qualities.get(self.quality_index)
    }

    fn set_quality(&mut self, quality_id: &str) {
        let Some(index) = self
            .qualities
            .iter()
            .position(|quality| quality.id == quality_id)
        else {
            return;
        };
        if let Err(error) = self.switch_quality_with(index, open_variant_session) {
            self.fail(error);
        }
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

struct HlsQualitySet {
    qualities: Vec<Quality>,
    urls: Vec<Url>,
    preferred_index: usize,
}

fn inspect_hls_qualities(master_url: &Url) -> Result<HlsQualitySet, String> {
    let inspected = oxideav_hls::inspect_hls(&hls_uri(master_url))
        .map_err(|error| format!("inspect HLS playlist qualities: {error}"))?;
    match inspected {
        HlsPlaylistInfo::Media { url } => Ok(HlsQualitySet {
            qualities: vec![Quality::new("hls-media", "HLS")],
            urls: vec![url],
            preferred_index: 0,
        }),
        HlsPlaylistInfo::Master {
            variants,
            preferred_variant,
        } => quality_set_from_variants(variants, preferred_variant),
    }
}

fn quality_set_from_variants(
    variants: Vec<HlsVariant>,
    preferred_variant: usize,
) -> Result<HlsQualitySet, String> {
    let preferred_url = variants
        .get(preferred_variant)
        .map(|variant| variant.url.clone())
        .ok_or_else(|| "HLS preferred variant index is out of range".to_owned())?;

    let video_variants: Vec<HlsVariant> = variants
        .into_iter()
        .filter(|variant| {
            variant.width.is_some_and(|width| width > 0)
                && variant.height.is_some_and(|height| height > 0)
        })
        .collect();
    if video_variants.is_empty() {
        return Err("HLS master contains no video variants with a declared resolution".into());
    }

    let preferred_index = video_variants
        .iter()
        .position(|variant| variant.url == preferred_url)
        .or_else(|| {
            video_variants
                .iter()
                .enumerate()
                .min_by_key(|(_, variant)| variant.bandwidth)
                .map(|(index, _)| index)
        })
        .unwrap_or(0);
    let mut qualities = Vec::with_capacity(video_variants.len());
    let mut urls = Vec::with_capacity(video_variants.len());
    for variant in video_variants {
        let base_label = variant_quality_name(&variant);
        let label = if variant.video_group.as_deref() == Some("chunked") {
            format!("{base_label} (Source)")
        } else {
            base_label.clone()
        };
        let mut id = base_label;
        if qualities.iter().any(|quality: &Quality| quality.id == id) {
            id = variant.url.as_str().to_owned();
        }
        eprintln!(
            "SanctuaryPlayer: HLS quality id={} label={} resolution={}x{} fps={} bandwidth={} variant={}",
            id,
            label,
            variant.width.unwrap_or(0),
            variant.height.unwrap_or(0),
            variant
                .frame_rate
                .map(|fps| format!("{fps:.3}"))
                .unwrap_or_else(|| "unknown".into()),
            variant.average_bandwidth.unwrap_or(variant.bandwidth),
            variant.url
        );
        qualities.push(Quality::new(id, label));
        urls.push(variant.url);
    }

    Ok(HlsQualitySet {
        qualities,
        urls,
        preferred_index,
    })
}

fn variant_quality_name(variant: &HlsVariant) -> String {
    if let Some(name) = variant
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.to_owned();
    }
    match (variant.height, variant.frame_rate) {
        (Some(height), Some(frame_rate)) if frame_rate.is_finite() && frame_rate > 0.0 => {
            format!("{height}p{}", frame_rate.round() as u32)
        }
        (Some(height), _) => format!("{height}p"),
        _ => format!("{} kbps", variant.bandwidth / 1000),
    }
}

fn hls_uri(url: &Url) -> String {
    format!("hls+{}", url.as_str())
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
                quality_urls: Vec::new(),
                quality_index: 0,
                active_quality_index: 0,
                decode_mode: DecodeMode::Cpu,
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

    fn hls_variant(
        url: &str,
        name: Option<&str>,
        video_group: Option<&str>,
        width: Option<u64>,
        height: Option<u64>,
        frame_rate: Option<f64>,
        bandwidth: u64,
    ) -> HlsVariant {
        HlsVariant {
            url: Url::parse(url).unwrap(),
            bandwidth,
            average_bandwidth: None,
            width,
            height,
            frame_rate,
            codecs: None,
            video_group: video_group.map(str::to_owned),
            audio_group: None,
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn hls_quality_list_exposes_video_variants_and_filters_audio_only() {
        let set = quality_set_from_variants(
            vec![
                hls_variant(
                    "https://example.test/source.m3u8",
                    Some("1080p60"),
                    Some("chunked"),
                    Some(1920),
                    Some(1080),
                    Some(60.0),
                    6_400_000,
                ),
                hls_variant(
                    "https://example.test/720.m3u8",
                    Some("720p60"),
                    Some("720p60"),
                    Some(1280),
                    Some(720),
                    Some(60.0),
                    3_400_000,
                ),
                hls_variant(
                    "https://example.test/audio.m3u8",
                    Some("Audio Only"),
                    Some("audio_only"),
                    None,
                    None,
                    None,
                    220_000,
                ),
            ],
            1,
        )
        .unwrap();

        assert_eq!(set.qualities.len(), 2);
        assert_eq!(set.qualities[0].id, "1080p60");
        assert_eq!(set.qualities[0].label, "1080p60 (Source)");
        assert_eq!(set.qualities[1].id, "720p60");
        assert_eq!(set.preferred_index, 1);
        assert_eq!(set.urls[1].as_str(), "https://example.test/720.m3u8");
    }

    #[test]
    fn hls_quality_label_falls_back_to_resolution_and_frame_rate() {
        let variant = hls_variant(
            "https://example.test/480.m3u8",
            None,
            None,
            Some(852),
            Some(480),
            Some(30.001),
            1_500_000,
        );
        assert_eq!(variant_quality_name(&variant), "480p30");
    }

    fn replacement_test_session() -> PlaybackSession {
        let (_tx, rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        PlaybackSession {
            rx,
            executor: None,
            video_stream: StreamInfo {
                index: 0,
                time_base: TimeBase::new(1, 90_000),
                duration: None,
                start_time: Some(0),
                params: CodecParameters::video(CodecId::new("h264")),
            },
            audio_stream: None,
            audio_output: None,
            duration: Some(Duration::from_secs(30)),
            rates: vec![0.5, 1.0, 2.0],
            timeline_origin_seconds: Some(0.0),
            first_video_seconds: Some(0.0),
            first_audio_seconds: None,
        }
    }

    #[test]
    fn quality_switch_recreates_session_and_resumes_previous_play_state() {
        let (mut playback, _old_tx) = clock_test_playback();
        playback.qualities = vec![
            Quality::new("1080p60", "1080p60 (Source)"),
            Quality::new("720p60", "720p60"),
        ];
        playback.quality_urls = vec![
            Url::parse("https://example.test/1080.m3u8").unwrap(),
            Url::parse("https://example.test/720.m3u8").unwrap(),
        ];
        playback.quality_index = 1;
        playback.active_quality_index = 1;
        playback.decode_mode = DecodeMode::VdpauDirect;
        playback.position = Duration::from_secs(12);
        playback
            .video_queue
            .push_back(FrameLease::from_frame(Frame::Video(VideoFrame {
                pts: Some(1_080_000),
                planes: Vec::new(),
            })));

        playback
            .switch_quality_with(0, |url, decode_mode| {
                assert_eq!(url.as_str(), "https://example.test/1080.m3u8");
                assert_eq!(decode_mode, DecodeMode::VdpauDirect);
                Ok(replacement_test_session())
            })
            .unwrap();

        assert_eq!(playback.quality().unwrap().id, "1080p60");
        assert_eq!(playback.quality_index, 0);
        assert_eq!(playback.active_quality_index, 0);
        assert_eq!(playback.position, Duration::ZERO);
        assert_eq!(playback.state, PlaybackState::Playing);
        assert!(playback.video_queue.is_empty());
        assert!(!playback.first_frame_presented);
        assert_eq!(playback.duration, Some(Duration::from_secs(30)));
        assert_eq!(playback.rates, vec![0.5, 1.0, 2.0]);
    }

    #[test]
    fn session_teardown_unblocks_a_sink_waiting_on_the_full_channel() {
        let (mut playback, _old_tx) = clock_test_playback();
        let (tx, rx) = mpsc::sync_channel(1);
        let mut sink = SessionSink::new(tx);
        sink.send(SessionMsg::Started(Vec::new())).unwrap();
        playback.rx = rx;

        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = sink.send(SessionMsg::Finished);
            let _ = done_tx.send(result.is_err());
        });

        std::thread::sleep(Duration::from_millis(10));
        playback.tear_down_session();

        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "dropping the receiver should disconnect a blocked sink send"
        );
        worker.join().unwrap();
    }

    #[test]
    fn quality_switch_keeps_a_paused_session_paused() {
        let (mut playback, _old_tx) = clock_test_playback();
        playback.state = PlaybackState::Paused;
        playback.qualities = vec![
            Quality::new("1080p60", "1080p60 (Source)"),
            Quality::new("720p60", "720p60"),
        ];
        playback.quality_urls = vec![
            Url::parse("https://example.test/1080.m3u8").unwrap(),
            Url::parse("https://example.test/720.m3u8").unwrap(),
        ];
        playback.quality_index = 1;
        playback.active_quality_index = 1;

        playback
            .switch_quality_with(0, |_url, _decode_mode| Ok(replacement_test_session()))
            .unwrap();

        assert_eq!(playback.state, PlaybackState::Paused);
        assert_eq!(playback.active_quality_index, 0);
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
