use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use ::oxideav::core::{
    CancellationToken, Error, Frame, FrameLease, MediaType, Packet, Rounding, StreamInfo, TimeBase,
    VideoColorInfo,
};
use ::oxideav::pipeline::{
    BarrierKind, ChannelCaps, CodecPreferences, EofMode, Executor, ExecutorHandle, Job, JobSink,
    PipelineStageInfo, TrackSink, TrackSinkInfo,
};
use oxideav_hls::{HlsPlaylistInfo, HlsVariant};
use serde_json::json;
use url::Url;

use crate::audio_output::AudioOutput;
use crate::audio_timeline::QueueResult;
use crate::model::{
    DebugEdge, DebugGraph, DebugGraphLane, DebugInfoSection, DebugNode, PlaybackState, Quality,
};
use crate::video::{VideoPlatform, VideoSource};

use super::{DecodeMode, PlaybackBackend, PlaybackWake, PlaybackWakeKind};

const SESSION_CHANNEL_CAP: usize = 2;
const VIDEO_QUEUE_CAP: usize = 2;
// Twitch MPEG-TS VODs can legally mux one track several seconds ahead of its
// sibling in physical packet order. OxideAV's default of 16 compressed packets
// per track is intentionally conservative for general pipelines, but it is too
// shallow for native playback: a full leading-track queue can block the shared
// demuxer before it reaches packets the lagging track needs now. Keep the
// decoded-video lookahead at two frames; this larger bound is compressed demux
// slack only. 256 packets covers the observed ~3.8 s skew while remaining
// strictly bounded per track.
const PLAYBACK_PACKET_CHANNEL_CAP: usize = 256;
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);
const DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(1);
const BUFFERING_GRACE: Duration = Duration::from_millis(250);
const TRACK_SINK_BACKPRESSURE_WAIT: Duration = Duration::from_millis(20);
static HTTP_RANGE_PROBE_CONFIG: Once = Once::new();

fn enable_http_range_probe() {
    HTTP_RANGE_PROBE_CONFIG.call_once(|| {
        let config = oxideav_http::HttpConfig::builder()
            .range_probe(true)
            .build();
        if let Err(error) = oxideav_http::install_default_config(config) {
            log::warn!("SanctuaryPlayer: could not enable HTTP range probing: {error}");
        }
    });
}

#[derive(Clone, Default)]
struct SessionChannelDepths {
    control: Arc<AtomicUsize>,
    audio: Arc<AtomicUsize>,
    video: Arc<AtomicUsize>,
}

impl SessionChannelDepths {
    fn for_kind(&self, kind: MediaType) -> Arc<AtomicUsize> {
        match kind {
            MediaType::Audio => Arc::clone(&self.audio),
            MediaType::Video => Arc::clone(&self.video),
            _ => Arc::clone(&self.control),
        }
    }

    fn current(&self, kind: MediaType) -> usize {
        let depth = match kind {
            MediaType::Audio => &self.audio,
            MediaType::Video => &self.video,
            _ => &self.control,
        };
        depth.load(Ordering::SeqCst).min(SESSION_CHANNEL_CAP)
    }
}

fn release_session_depth(depth: &AtomicUsize) {
    let _ = depth.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
        Some(value.saturating_sub(1))
    });
}

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
    master_url: Url,
    decode_mode: DecodeMode,
    muted: bool,
    volume: f32,
    control_rx: Receiver<SessionMsg>,
    audio_rx: Receiver<SessionMsg>,
    video_rx: Receiver<SessionMsg>,
    channel_depths: SessionChannelDepths,
    executor: Option<ExecutorHandle>,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    video_decoder: Option<DecoderDebugInfo>,
    audio_decoder: Option<DecoderDebugInfo>,
    audio_output: Option<AudioOutput>,
    pending_audio_frame: Option<FrameLease>,
    video_queue: VecDeque<FrameLease>,
    video_clock: VideoClock,
    timeline_origin_seconds: Option<f64>,
    first_video_seconds: Option<f64>,
    first_audio_seconds: Option<f64>,
    audio_anchor_seconds: Option<f64>,
    first_frame_presented: bool,
    starvation_started_at: Option<Instant>,
    sink_finished: bool,
    video_eof: bool,
    audio_eof: bool,
    seek_pending: Option<PendingSeek>,
    post_seek_epoch: Option<PostSeekEpoch>,
    queued_seek: Option<Duration>,
    seek_supported: bool,
    diagnostics: PlaybackDiagnostics,
}

#[derive(Clone, Copy, Debug, Default)]
struct VideoClock {
    origin: Option<(i64, Instant)>,
    frozen_pts: Option<i64>,
}

impl VideoClock {
    fn reset(&mut self) {
        self.origin = None;
        self.frozen_pts = None;
    }

    fn establish(&mut self, pts: i64, now: Instant, playing: bool) {
        if playing {
            self.origin = Some((pts, now));
            self.frozen_pts = None;
        } else {
            self.origin = None;
            self.frozen_pts = Some(pts);
        }
    }

    fn play(&mut self, now: Instant) {
        if self.origin.is_none()
            && let Some(pts) = self.frozen_pts.take()
        {
            self.origin = Some((pts, now));
        }
    }

    fn pause(&mut self, now: Instant, time_base: TimeBase) {
        if let Some(pts) = self.pts_at(now, time_base) {
            self.origin = None;
            self.frozen_pts = Some(pts);
        }
    }

    fn pts_at(&self, now: Instant, time_base: TimeBase) -> Option<i64> {
        if let Some((origin_pts, origin_time)) = self.origin {
            let elapsed = now.saturating_duration_since(origin_time);
            let nanos = i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX);
            let delta = TimeBase::NANOS.rescale_rnd(nanos, time_base, Rounding::Floor);
            origin_pts.checked_add(delta)
        } else {
            self.frozen_pts
        }
    }

    fn deadline_for(&self, pts: i64, time_base: TimeBase) -> Option<Instant> {
        let (origin_pts, origin_time) = self.origin?;
        let delta = pts.checked_sub(origin_pts)?;
        if delta <= 0 {
            return Some(origin_time);
        }
        let nanos = time_base.rescale_rnd(delta, TimeBase::NANOS, Rounding::Ceil);
        let nanos = u64::try_from(nanos).ok()?;
        origin_time.checked_add(Duration::from_nanos(nanos))
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingSeek {
    generation: u32,
    requested: Duration,
    prior_position: Duration,
    resume_playing: bool,
    barriers_remaining: usize,
    landing: Option<(i64, TimeBase)>,
    rejected: bool,
}

#[derive(Clone, Copy, Debug)]
struct PostSeekEpoch {
    floor: Duration,
    audio_aligned: bool,
    video_aligned: bool,
    dropped_audio_frames: u64,
    dropped_video_frames: u64,
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
    StreamUpdate(Box<StreamInfo>),
    Frame {
        kind: MediaType,
        frame: FrameLease,
    },
    Barrier {
        kind: Option<MediaType>,
        barrier: BarrierKind,
    },
    EndOfStream(MediaType),
    Finished,
}

impl SessionMsg {
    fn label(&self) -> &'static str {
        match self {
            Self::Started(_) => "start",
            Self::StreamUpdate(_) => "stream-update",
            Self::Frame {
                kind: MediaType::Audio,
                ..
            } => "audio",
            Self::Frame {
                kind: MediaType::Video,
                ..
            } => "video",
            Self::Frame { .. } => "other",
            Self::Barrier { .. } => "barrier",
            Self::EndOfStream(_) => "end-of-stream",
            Self::Finished => "finish",
        }
    }

    fn wake_kind(&self) -> PlaybackWakeKind {
        match self {
            Self::Frame {
                kind: MediaType::Audio,
                ..
            } => PlaybackWakeKind::Audio,
            Self::Frame {
                kind: MediaType::Video,
                ..
            } => PlaybackWakeKind::Video,
            _ => PlaybackWakeKind::Control,
        }
    }
}

struct SessionSink {
    control_tx: SyncSender<SessionMsg>,
    audio_tx: SyncSender<SessionMsg>,
    video_tx: SyncSender<SessionMsg>,
    depths: SessionChannelDepths,
    wake: PlaybackWake,
}

impl SessionSink {
    fn new(
        control_tx: SyncSender<SessionMsg>,
        audio_tx: SyncSender<SessionMsg>,
        video_tx: SyncSender<SessionMsg>,
        depths: SessionChannelDepths,
        wake: PlaybackWake,
    ) -> Self {
        Self {
            control_tx,
            audio_tx,
            video_tx,
            depths,
            wake,
        }
    }

    fn send_control(&mut self, message: SessionMsg) -> ::oxideav::core::Result<()> {
        let wake_kind = message.wake_kind();
        self.depths.control.fetch_add(1, Ordering::SeqCst);
        if self.control_tx.send(message).is_err() {
            release_session_depth(&self.depths.control);
            return Err(Error::other("SanctuaryPlayer: control receiver dropped"));
        }
        self.wake.wake(wake_kind);
        Ok(())
    }

    fn send_legacy_track_message(
        &mut self,
        kind: MediaType,
        message: SessionMsg,
    ) -> ::oxideav::core::Result<()> {
        let (tx, depth, wake_kind, label) = match kind {
            MediaType::Audio => (
                &self.audio_tx,
                &self.depths.audio,
                PlaybackWakeKind::Audio,
                "audio",
            ),
            MediaType::Video => (
                &self.video_tx,
                &self.depths.video,
                PlaybackWakeKind::Video,
                "video",
            ),
            _ => return Ok(()),
        };
        depth.fetch_add(1, Ordering::SeqCst);
        if tx.send(message).is_err() {
            release_session_depth(depth);
            return Err(Error::other(format!(
                "SanctuaryPlayer: {label} receiver dropped"
            )));
        }
        self.wake.wake(wake_kind);
        Ok(())
    }
}

struct SessionTrackSink {
    kind: MediaType,
    tx: SyncSender<SessionMsg>,
    depth: Arc<AtomicUsize>,
    wake: PlaybackWake,
    cancellation: CancellationToken,
    blocked_sends: u64,
    last_backpressure_log: Option<Instant>,
}

impl SessionTrackSink {
    fn new(
        kind: MediaType,
        tx: SyncSender<SessionMsg>,
        depth: Arc<AtomicUsize>,
        wake: PlaybackWake,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            kind,
            tx,
            depth,
            wake,
            cancellation,
            blocked_sends: 0,
            last_backpressure_log: None,
        }
    }

    fn wake_kind(&self) -> PlaybackWakeKind {
        match self.kind {
            MediaType::Audio => PlaybackWakeKind::Audio,
            MediaType::Video => PlaybackWakeKind::Video,
            _ => PlaybackWakeKind::Control,
        }
    }

    fn send(&mut self, mut message: SessionMsg) -> ::oxideav::core::Result<()> {
        let label = message.label();
        let wake_kind = self.wake_kind();
        let mut wake_sent_while_blocked = false;
        loop {
            if self.cancellation.is_cancelled() {
                return Err(Error::cancelled(format!(
                    "SanctuaryPlayer: {label} TrackSink send cancelled"
                )));
            }
            self.depth.fetch_add(1, Ordering::SeqCst);
            match self.tx.try_send(message) {
                Ok(()) => {
                    self.wake.wake(wake_kind);
                    return Ok(());
                }
                Err(TrySendError::Full(returned)) => {
                    release_session_depth(&self.depth);
                    message = returned;
                    self.blocked_sends = self.blocked_sends.saturating_add(1);
                    let now = Instant::now();
                    if self
                        .last_backpressure_log
                        .is_none_or(|last| now.duration_since(last) >= DIAGNOSTIC_INTERVAL)
                    {
                        log::info!(
                            "SanctuaryPlayer: TrackSink backpressure kind={:?} waiting_for={} blocked_sends={}",
                            self.kind,
                            label,
                            self.blocked_sends
                        );
                        self.last_backpressure_log = Some(now);
                    }
                    // Wake once when this message first encounters
                    // backpressure so the event loop can consume the bounded
                    // channel. After that, avoid repeatedly waking/repainting
                    // a paused player while we wait. The bounded poll keeps
                    // cancellation responsive without exposing WouldBlock
                    // through the OxideAV sink contract.
                    if !wake_sent_while_blocked {
                        self.wake.wake(wake_kind);
                        wake_sent_while_blocked = true;
                    }
                    std::thread::sleep(TRACK_SINK_BACKPRESSURE_WAIT);
                }
                Err(TrySendError::Disconnected(_)) => {
                    release_session_depth(&self.depth);
                    if self.cancellation.is_cancelled() {
                        return Err(Error::cancelled(format!(
                            "SanctuaryPlayer: {label} TrackSink receiver dropped during cancellation"
                        )));
                    }
                    return Err(Error::other(format!(
                        "SanctuaryPlayer: {label} TrackSink receiver dropped"
                    )));
                }
            }
        }
    }
}

impl TrackSink for SessionTrackSink {
    fn write_packet(
        &mut self,
        _stream_index: u32,
        _kind: MediaType,
        _packet: Packet,
    ) -> ::oxideav::core::Result<()> {
        Err(Error::unsupported(
            "SanctuaryPlayer playback TrackSink requires decoded frames",
        ))
    }

    fn write_frame_lease(
        &mut self,
        _stream_index: u32,
        kind: MediaType,
        frame: FrameLease,
    ) -> ::oxideav::core::Result<()> {
        if kind != self.kind {
            return Err(Error::other(format!(
                "SanctuaryPlayer: TrackSink kind mismatch expected={:?} got={kind:?}",
                self.kind
            )));
        }
        self.send(SessionMsg::Frame { kind, frame })
    }

    fn stream_update(&mut self, stream: &StreamInfo) -> ::oxideav::core::Result<()> {
        self.send(SessionMsg::StreamUpdate(Box::new(stream.clone())))
    }

    fn barrier(&mut self, barrier: BarrierKind) -> ::oxideav::core::Result<()> {
        self.send(SessionMsg::Barrier {
            kind: Some(self.kind),
            barrier,
        })
    }

    fn end_of_stream(
        &mut self,
        _stream_index: u32,
        kind: MediaType,
    ) -> ::oxideav::core::Result<()> {
        self.send(SessionMsg::EndOfStream(kind))
    }
}

impl JobSink for SessionSink {
    fn start(&mut self, streams: &[StreamInfo]) -> ::oxideav::core::Result<()> {
        self.send_control(SessionMsg::Started(streams.to_vec()))
    }

    fn open_track_sinks(
        &mut self,
        tracks: &[TrackSinkInfo],
        cancellation: CancellationToken,
    ) -> ::oxideav::core::Result<Option<Vec<Box<dyn TrackSink + Send>>>> {
        let mut sinks: Vec<Box<dyn TrackSink + Send>> = Vec::with_capacity(tracks.len());
        for track in tracks {
            let kind = track.stream.params.media_type;
            let tx = match kind {
                MediaType::Audio => self.audio_tx.clone(),
                MediaType::Video => self.video_tx.clone(),
                other => {
                    return Err(Error::unsupported(format!(
                        "SanctuaryPlayer: unsupported playback TrackSink media type {other:?}"
                    )));
                }
            };
            sinks.push(Box::new(SessionTrackSink::new(
                kind,
                tx,
                self.depths.for_kind(kind),
                self.wake.clone(),
                cancellation.clone(),
            )));
        }
        Ok(Some(sinks))
    }

    fn stream_update(&mut self, stream: &StreamInfo) -> ::oxideav::core::Result<()> {
        self.send_legacy_track_message(
            stream.params.media_type,
            SessionMsg::StreamUpdate(Box::new(stream.clone())),
        )
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
        self.send_legacy_track_message(kind, SessionMsg::Frame { kind, frame })
    }

    fn barrier(&mut self, barrier: BarrierKind) -> ::oxideav::core::Result<()> {
        self.send_control(SessionMsg::Barrier {
            kind: None,
            barrier,
        })
    }

    fn end_of_stream(
        &mut self,
        _stream_index: u32,
        kind: MediaType,
    ) -> ::oxideav::core::Result<()> {
        self.send_control(SessionMsg::EndOfStream(kind))
    }

    fn finish(&mut self) -> ::oxideav::core::Result<()> {
        self.send_control(SessionMsg::Finished)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DecoderDebugInfo {
    implementation: String,
    hardware_accelerated: bool,
}

struct PlaybackSession {
    control_rx: Receiver<SessionMsg>,
    audio_rx: Receiver<SessionMsg>,
    video_rx: Receiver<SessionMsg>,
    channel_depths: SessionChannelDepths,
    executor: Option<ExecutorHandle>,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    video_decoder: Option<DecoderDebugInfo>,
    audio_decoder: Option<DecoderDebugInfo>,
    audio_output: Option<AudioOutput>,
    duration: Option<Duration>,
    rates: Vec<f32>,
    timeline_origin_seconds: Option<f64>,
    first_video_seconds: Option<f64>,
    first_audio_seconds: Option<f64>,
}

fn selected_decoder_info(
    executor: &ExecutorHandle,
    media_type: MediaType,
) -> Option<DecoderDebugInfo> {
    executor
        .pipeline_tracks()
        .iter()
        .find(|track| track.media_type == media_type)
        .and_then(|track| track.decoder.as_ref())
        .map(|caps| DecoderDebugInfo {
            implementation: caps.implementation.clone(),
            hardware_accelerated: caps.hardware_accelerated,
        })
}
fn open_variant_session(
    variant_url: &Url,
    decode_mode: DecodeMode,
    include_audio: bool,
    wake: PlaybackWake,
    cancellation: CancellationToken,
) -> Result<PlaybackSession, String> {
    decode_mode.validate_current_platform()?;
    let input = hls_uri(variant_url);
    let display = if include_audio {
        json!({
            "audio": [{ "from": "@in" }],
            "video": [{ "from": "@in" }]
        })
    } else {
        json!({ "video": [{ "from": "@in" }] })
    };
    let job_json = serde_json::to_string(&json!({
        "@in": { "all": [{ "from": input }] },
        "@display": display,
    }))
    .map_err(|error| format!("build OxideAV playback job: {error}"))?;
    let job = Job::from_json(&job_json).map_err(|error| error.to_string())?;
    job.validate().map_err(|error| error.to_string())?;

    let mut registries = ::oxideav::Registries::new();
    oxideav_meta::register_all(&mut registries);
    #[cfg(target_os = "windows")]
    crate::vulkan_video_decoder::register(&mut registries);
    #[cfg(target_os = "android")]
    if option_env!("SANCTUARY_ANDROID_DISABLE_MEDIACODEC").is_none() {
        oxideav_mediacodec::register(&mut registries);
    } else {
        log::info!("SanctuaryPlayer: MediaCodec registration disabled by validation build");
    }

    let codec_preferences = codec_preferences(decode_mode);
    let (control_tx, control_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
    let (audio_tx, audio_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
    let (video_tx, video_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
    let channel_depths = SessionChannelDepths::default();
    let sink = Box::new(SessionSink::new(
        control_tx,
        audio_tx,
        video_tx,
        channel_depths.clone(),
        wake,
    ));
    log::info!(
        "SanctuaryPlayer: OxideAV compressed packet queue cap={} per track",
        PLAYBACK_PACKET_CHANNEL_CAP
    );
    let executor = Executor::new(&job, &registries)
        .with_sink_override("@display", sink)
        .with_codec_preferences(codec_preferences)
        .with_channel_caps(ChannelCaps {
            packets: PLAYBACK_PACKET_CHANNEL_CAP,
            ..ChannelCaps::default()
        })
        .with_eof_mode(EofMode::WaitForSeek)
        .with_cancellation_token(cancellation.clone())
        .with_threads(0)
        .spawn()
        .map_err(|error| format!("start OxideAV playback: {error}"))?;
    let video_decoder = selected_decoder_info(&executor, MediaType::Video);
    let audio_decoder = selected_decoder_info(&executor, MediaType::Audio);
    log::info!(
        "SanctuaryPlayer: selected OxideAV decoders video={} ({}) audio={} ({})",
        video_decoder
            .as_ref()
            .map(|decoder| decoder.implementation.as_str())
            .unwrap_or("none"),
        video_decoder
            .as_ref()
            .map(|decoder| if decoder.hardware_accelerated {
                "hardware"
            } else {
                "software"
            })
            .unwrap_or("unknown"),
        audio_decoder
            .as_ref()
            .map(|decoder| decoder.implementation.as_str())
            .unwrap_or("none"),
        audio_decoder
            .as_ref()
            .map(|decoder| if decoder.hardware_accelerated {
                "hardware"
            } else {
                "software"
            })
            .unwrap_or("unknown"),
    );

    let streams = match control_rx.recv_timeout(OPEN_TIMEOUT) {
        Ok(SessionMsg::Started(streams)) => {
            release_session_depth(&channel_depths.control);
            streams
        }
        Ok(_) => {
            release_session_depth(&channel_depths.control);
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

    // Decoder output metadata may still be provisional here. In-band
    // configured codecs such as MPEG-TS/ADTS AAC only learn their true PCM
    // rate/channel shape after the first packet, so defer opening sysaudio
    // until the ordered decoder StreamUpdate arrives.
    let audio_output = None;
    let duration = streams.iter().filter_map(stream_duration).max();
    let first_video_seconds = stream_start_seconds(&video_stream);
    let first_audio_seconds = audio_stream.as_ref().and_then(stream_start_seconds);
    let timeline_origin_seconds = timeline_origin_seconds(
        first_video_seconds,
        first_audio_seconds,
        audio_stream.is_some(),
    );

    log::info!(
        "SanctuaryPlayer: OxideAV video stream mode={} codec={} {}x{} time_base={}/{}",
        decode_mode,
        video_stream.params.codec_id,
        video_stream.params.width.unwrap_or(0),
        video_stream.params.height.unwrap_or(0),
        video_stream.time_base.num(),
        video_stream.time_base.den(),
    );
    if let Some(stream) = audio_stream.as_ref() {
        log::info!(
            "SanctuaryPlayer: OxideAV provisional audio stream codec={} rate={:?}Hz channels={:?} format={:?} time_base={}/{}",
            stream.params.codec_id,
            stream.params.sample_rate,
            stream.params.resolved_channels(),
            stream.params.sample_format,
            stream.time_base.num(),
            stream.time_base.den(),
        );
    }

    let rates = vec![1.0];

    Ok(PlaybackSession {
        control_rx,
        audio_rx,
        video_rx,
        channel_depths,
        executor: Some(executor),
        video_stream,
        audio_stream,
        video_decoder,
        audio_decoder,
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
        initial_qualities: &str,
        decode_mode: DecodeMode,
        muted: bool,
        wake: PlaybackWake,
        cancellation: CancellationToken,
    ) -> Result<Self, String> {
        // HLS segment opens share oxideav-http's process-wide source. The
        // fallback probes only when HEAD cannot establish a seekable length.
        enable_http_range_probe();
        let quality_set = inspect_hls_qualities(&m3u8_url, &cancellation)?;
        if cancellation.is_cancelled() {
            return Err("OxideAV open cancelled after HLS inspection".into());
        }
        let initial_quality_index = select_initial_quality_index(
            &quality_set.qualities,
            quality_set.preferred_index,
            initial_qualities,
        );
        let selected_url = quality_set.urls[initial_quality_index].clone();
        log::info!(
            "SanctuaryPlayer: HLS initial quality={} variant={}",
            quality_set.qualities[initial_quality_index].label,
            selected_url
        );
        let session = open_variant_session(
            &selected_url,
            decode_mode,
            // YouTube's selected video playlist has separate audio in the
            // master. Until HLS can merge that rendition, request video only.
            source.platform != VideoPlatform::YouTube,
            wake.clone(),
            cancellation.clone(),
        )?;

        Ok(Self {
            source,
            state: PlaybackState::Paused,
            position: Duration::ZERO,
            duration: session.duration,
            rate: 1.0,
            rates: session.rates,
            qualities: quality_set.qualities,
            quality_urls: quality_set.urls,
            quality_index: initial_quality_index,
            master_url: m3u8_url,
            decode_mode,
            muted,
            volume: 1.0,
            control_rx: session.control_rx,
            audio_rx: session.audio_rx,
            video_rx: session.video_rx,
            channel_depths: session.channel_depths,
            executor: session.executor,
            video_stream: session.video_stream,
            audio_stream: session.audio_stream,
            video_decoder: session.video_decoder,
            audio_decoder: session.audio_decoder,
            audio_output: session.audio_output,
            pending_audio_frame: None,
            video_queue: VecDeque::new(),
            video_clock: VideoClock::default(),
            timeline_origin_seconds: session.timeline_origin_seconds,
            first_video_seconds: session.first_video_seconds,
            first_audio_seconds: session.first_audio_seconds,
            audio_anchor_seconds: session.first_audio_seconds,
            first_frame_presented: false,
            starvation_started_at: None,
            sink_finished: false,
            video_eof: false,
            audio_eof: false,
            seek_pending: None,
            post_seek_epoch: None,
            queued_seek: None,
            seek_supported: true,
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
        if self.seek_pending.is_some() {
            // A seek barrier is ordered behind all pre-seek payload on each
            // routed track. Drain unconditionally until every matching route
            // barrier arrives; ordinary A/V forward-buffer limits would
            // otherwise be able to hide the barrier behind stale frames.
            return None;
        }
        if self.pending_audio_frame.is_some() {
            return Some("audio-frame-pending");
        }
        if self.audio_stream.is_some() && self.audio_output.is_none() {
            // Keep draining until the decoder publishes authoritative PCM
            // metadata; otherwise the video queue could fill first and hide
            // the StreamUpdate behind shared-channel back-pressure.
            return None;
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
            VIDEO_QUEUE_CAP
        };
        (self.video_queue.len() >= target).then_some("video-buffer-ready")
    }

    fn should_pump_audio_channel(&self) -> bool {
        if matches!(self.state, PlaybackState::Error(_)) {
            return false;
        }
        if self.seek_pending.is_some() {
            return true;
        }
        if self.pending_audio_frame.is_some() {
            return false;
        }
        if self.audio_stream.is_some() && self.audio_output.is_none() {
            return true;
        }
        let Some(audio) = self.audio_output.as_ref() else {
            return true;
        };
        if audio.headroom_samples() < audio.minimum_headroom_samples() {
            return false;
        }
        if !audio.preroll_ready() {
            return true;
        }
        if matches!(self.state, PlaybackState::Paused) {
            return false;
        }
        audio.queued_samples() < audio.queue_target_samples()
    }

    fn video_pump_limit(&self) -> usize {
        if self.seek_pending.is_some() {
            usize::MAX
        } else if matches!(self.state, PlaybackState::Paused) {
            usize::from(self.video_queue.is_empty())
        } else {
            VIDEO_QUEUE_CAP
        }
    }

    fn pump_session(&mut self) {
        if let Some(frame) = self.pending_audio_frame.take() {
            match self.queue_audio_frame(frame) {
                Ok(Some(frame)) => self.pending_audio_frame = Some(frame),
                Ok(None) => {}
                Err(error) => {
                    self.fail(error);
                    return;
                }
            }
        }

        while !self.sink_finished && !matches!(self.state, PlaybackState::Error(_)) {
            match self.control_rx.try_recv() {
                Ok(message) => {
                    release_session_depth(&self.channel_depths.control);
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

        while self.should_pump_audio_channel() {
            match self.audio_rx.try_recv() {
                Ok(message) => {
                    release_session_depth(&self.channel_depths.audio);
                    if let Err(error) = self.handle_session_message(message) {
                        self.fail(error);
                        break;
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        loop {
            if self.video_queue.len() >= self.video_pump_limit() {
                break;
            }
            match self.video_rx.try_recv() {
                Ok(message) => {
                    release_session_depth(&self.channel_depths.video);
                    if let Err(error) = self.handle_session_message(message) {
                        self.fail(error);
                        break;
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        self.collect_executor_result();
        self.update_end_state();
    }

    fn handle_session_message(&mut self, message: SessionMsg) -> Result<(), String> {
        match message {
            SessionMsg::Started(_) => Ok(()),
            SessionMsg::StreamUpdate(stream) => self.handle_stream_update(*stream),
            SessionMsg::Frame { kind, frame } => {
                if self.seek_pending.is_some() {
                    if kind == MediaType::Video {
                        self.diagnostics.dropped_video_frames =
                            self.diagnostics.dropped_video_frames.saturating_add(1);
                    }
                    return Ok(());
                }
                match kind {
                    MediaType::Video => {
                        self.diagnostics.received_video_frames =
                            self.diagnostics.received_video_frames.saturating_add(1);
                        if self.drop_before_post_seek_epoch(kind, frame.pts()) {
                            return Ok(());
                        }
                        self.observe_frame_timestamp(kind, frame.pts());
                        self.sync_audio_origin()?;
                        if frame.pts().is_none() {
                            self.diagnostics.dropped_video_frames =
                                self.diagnostics.dropped_video_frames.saturating_add(1);
                            if self.diagnostics.dropped_video_frames <= 3
                                || self.diagnostics.dropped_video_frames.is_multiple_of(60)
                            {
                                log::info!(
                                    "SanctuaryPlayer: dropping decoded video frame with no PTS dropped={}",
                                    self.diagnostics.dropped_video_frames
                                );
                            }
                            return Ok(());
                        }
                        if self.queue_video_frame(frame).is_some() {
                            return Err(
                                "video TrackSink delivered beyond the configured presentation lookahead"
                                    .to_owned(),
                            );
                        }
                        Ok(())
                    }
                    MediaType::Audio => {
                        self.diagnostics.received_audio_frames =
                            self.diagnostics.received_audio_frames.saturating_add(1);
                        if self.drop_before_post_seek_epoch(kind, frame.pts()) {
                            return Ok(());
                        }
                        self.observe_frame_timestamp(kind, frame.pts());
                        self.sync_audio_origin()?;
                        self.pending_audio_frame = self.queue_audio_frame(frame)?;
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            SessionMsg::Barrier { kind, barrier } => self.handle_seek_barrier(kind, barrier),
            SessionMsg::EndOfStream(kind) => {
                match kind {
                    MediaType::Video => self.video_eof = true,
                    MediaType::Audio => {
                        self.audio_eof = true;
                        if let Some(audio) = self.audio_output.as_mut() {
                            audio.finish_input()?;
                        }
                    }
                    _ => {}
                }
                log::info!("SanctuaryPlayer: track reached end-of-stream kind={kind:?}");
                Ok(())
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

    fn handle_stream_update(&mut self, stream: StreamInfo) -> Result<(), String> {
        self.handle_stream_update_with(stream, |stream, muted| {
            AudioOutput::open(&stream.params, stream.time_base, muted)
        })
    }

    fn handle_stream_update_with<F>(
        &mut self,
        stream: StreamInfo,
        open_audio: F,
    ) -> Result<(), String>
    where
        F: FnOnce(&StreamInfo, bool) -> Result<AudioOutput, String>,
    {
        match stream.params.media_type {
            MediaType::Video => {
                self.video_stream = stream;
                Ok(())
            }
            MediaType::Audio => {
                if let Some(previous) = self.audio_stream.as_ref()
                    && self.audio_output.is_some()
                    && !previous.params.matches_core(&stream.params)
                {
                    return Err(format!(
                        "decoded audio format changed after output opened: {:?} -> {:?}",
                        previous.params, stream.params
                    ));
                }

                self.audio_stream = Some(stream.clone());
                let complete = audio_stream_is_authoritative(&stream);
                if !complete {
                    log::info!(
                        "SanctuaryPlayer: decoder audio stream update remains provisional codec={} rate={:?}Hz channels={:?} format={:?}",
                        stream.params.codec_id,
                        stream.params.sample_rate,
                        stream.params.resolved_channels(),
                        stream.params.sample_format,
                    );
                    return Ok(());
                }

                log::info!(
                    "SanctuaryPlayer: authoritative audio stream codec={} rate={}Hz channels={} format={:?} time_base={}/{}",
                    stream.params.codec_id,
                    stream.params.sample_rate.unwrap_or(0),
                    stream.params.resolved_channels().unwrap_or(0),
                    stream.params.sample_format,
                    stream.time_base.num(),
                    stream.time_base.den(),
                );

                if self.audio_output.is_none() {
                    let mut audio = open_audio(&stream, self.muted)
                        .map_err(|error| format!("open authoritative audio output: {error}"))?;
                    audio.set_volume(if self.muted { 0.0 } else { self.volume });
                    audio.set_paused(!matches!(self.state, PlaybackState::Playing))?;
                    self.audio_output = Some(audio);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn queue_video_frame_at(&mut self, frame: FrameLease, now: Instant) -> Option<FrameLease> {
        if self.video_queue.len() >= VIDEO_QUEUE_CAP
            && let Some(desired_pts) = self.video_clock.pts_at(now, self.video_stream.time_base)
        {
            while self.video_queue.len() >= VIDEO_QUEUE_CAP
                && self
                    .video_queue
                    .get(1)
                    .and_then(FrameLease::pts)
                    .is_some_and(|pts| pts <= desired_pts)
            {
                self.video_queue.pop_front();
                self.diagnostics.dropped_video_frames =
                    self.diagnostics.dropped_video_frames.saturating_add(1);
            }
        }

        if self.video_queue.len() >= VIDEO_QUEUE_CAP {
            Some(frame)
        } else {
            self.video_queue.push_back(frame);
            None
        }
    }

    fn queue_video_frame(&mut self, frame: FrameLease) -> Option<FrameLease> {
        self.queue_video_frame_at(frame, Instant::now())
    }

    fn queue_audio_frame(&mut self, frame: FrameLease) -> Result<Option<FrameLease>, String> {
        let audio = self
            .audio_output
            .as_mut()
            .ok_or_else(|| "OxideAV produced audio without an audio output".to_owned())?;
        let audio_frame = match frame.as_frame() {
            Some(Frame::Audio(audio_frame)) => audio_frame,
            _ => return Err("OxideAV audio output was not an owned AudioFrame".to_owned()),
        };
        match audio.queue(audio_frame)? {
            QueueResult::Queued | QueueResult::Dropped => Ok(None),
            QueueResult::Deferred => Ok(Some(frame)),
        }
    }

    fn handle_seek_barrier(
        &mut self,
        kind: Option<MediaType>,
        barrier: BarrierKind,
    ) -> Result<(), String> {
        let generation = match barrier {
            BarrierKind::SeekFlush { generation, .. }
            | BarrierKind::SeekRejected { generation } => generation,
        };
        let Some(pending) = self.seek_pending.as_mut() else {
            log::info!("SanctuaryPlayer: ignoring stale seek barrier generation={generation}");
            return Ok(());
        };
        if generation != pending.generation {
            log::info!(
                "SanctuaryPlayer: ignoring stale seek barrier generation={} current={}",
                generation,
                pending.generation
            );
            return Ok(());
        }

        match barrier {
            BarrierKind::SeekFlush {
                landed_pts,
                time_base,
                ..
            } => {
                match kind {
                    Some(MediaType::Video) => self.video_eof = false,
                    Some(MediaType::Audio) => self.audio_eof = false,
                    None => {
                        self.video_eof = false;
                        self.audio_eof = false;
                    }
                    Some(_) => {}
                }
                pending.landing.get_or_insert((landed_pts, time_base));
            }
            BarrierKind::SeekRejected { .. } => pending.rejected = true,
        }
        pending.barriers_remaining = pending.barriers_remaining.saturating_sub(1);
        if pending.barriers_remaining != 0 {
            return Ok(());
        }

        let pending = self
            .seek_pending
            .take()
            .expect("matching seek barrier implies pending seek");
        if pending.rejected {
            self.seek_supported = false;
            self.queued_seek = None;
            self.position = pending.prior_position;
            self.state = PlaybackState::Paused;
            if pending.resume_playing {
                self.play();
            }
            log::info!(
                "SanctuaryPlayer: seek rejected generation={} restored={:.3}s",
                pending.generation,
                self.position.as_secs_f64()
            );
            return Ok(());
        }

        let (landed_pts, time_base) = pending
            .landing
            .ok_or_else(|| "OxideAV seek completed without a landing timestamp".to_owned())?;
        let origin = self.timeline_origin_seconds.ok_or_else(|| {
            "OxideAV seek landed before media timeline origin was known".to_owned()
        })?;
        let raw_landed_seconds = time_base.seconds_of(landed_pts);
        if !raw_landed_seconds.is_finite() {
            return Err("OxideAV seek returned a non-finite landing timestamp".into());
        }
        let landed = Duration::from_secs_f64((raw_landed_seconds - origin).max(0.0));

        if let Some(target) = self.queued_seek.take()
            && target != pending.requested
        {
            // The active source operation has now finished, so there can be at
            // most one more physical seek to perform. Keep audio paused and do
            // not rebuild/preroll intermediate A/V state that will immediately
            // be discarded again.
            log::info!(
                "SanctuaryPlayer: seek generation={} superseded after landing={:.3}s; dispatching latest={:.3}s",
                pending.generation,
                landed.as_secs_f64(),
                target.as_secs_f64(),
            );
            self.dispatch_seek(target, pending.prior_position, pending.resume_playing)?;
            return Ok(());
        }

        if let Some(audio) = self.audio_output.as_mut() {
            audio.set_paused(true)?;
        }
        // The first post-barrier decoded audio PTS at or after the video-defined
        // landing establishes the new audio epoch. Valid MPEG-TS can place older
        // audio later in physical byte order.
        self.audio_anchor_seconds = None;
        self.post_seek_epoch = Some(PostSeekEpoch {
            floor: landed,
            audio_aligned: self.audio_stream.is_none(),
            video_aligned: false,
            dropped_audio_frames: 0,
            dropped_video_frames: 0,
        });
        if let Some(stream) = self.audio_stream.as_ref() {
            if audio_stream_is_authoritative(stream) {
                let audio = AudioOutput::open(&stream.params, stream.time_base, self.muted)
                    .map_err(|error| format!("reopen audio output after seek: {error}"))?;
                audio.set_volume(if self.muted { 0.0 } else { self.volume });
                self.audio_output = Some(audio);
            } else {
                // A freshly opened rendition can complete its seek before AAC
                // has decoded enough data to publish rate/channels. The first
                // ordered post-seek StreamUpdate will open AudioOutput.
                self.audio_output = None;
                log::info!(
                    "SanctuaryPlayer: seek completed before authoritative audio metadata; waiting for decoder stream update"
                );
            }
        }

        self.video_queue.clear();
        self.video_clock.reset();
        self.first_frame_presented = false;
        self.position = landed;
        self.state = PlaybackState::Paused;
        if pending.resume_playing {
            self.play();
        }
        log::info!(
            "SanctuaryPlayer: seek landed generation={} requested={:.3}s landed={:.3}s raw={:.3}s resume_playing={}",
            pending.generation,
            pending.requested.as_secs_f64(),
            landed.as_secs_f64(),
            raw_landed_seconds,
            pending.resume_playing,
        );
        Ok(())
    }

    fn drop_before_post_seek_epoch(&mut self, kind: MediaType, pts: Option<i64>) -> bool {
        let Some(epoch) = self.post_seek_epoch else {
            return false;
        };
        let aligned = match kind {
            MediaType::Audio => epoch.audio_aligned,
            MediaType::Video => epoch.video_aligned,
            _ => return false,
        };
        if aligned {
            return false;
        }

        let position = self.frame_position_for_kind(kind, pts);
        let Some(position) = position else {
            if kind == MediaType::Audio {
                if let Some(epoch) = self.post_seek_epoch.as_mut() {
                    epoch.dropped_audio_frames = epoch.dropped_audio_frames.saturating_add(1);
                }
                return true;
            }
            return false;
        };

        if position < epoch.floor {
            match kind {
                MediaType::Audio => {
                    if let Some(epoch) = self.post_seek_epoch.as_mut() {
                        epoch.dropped_audio_frames = epoch.dropped_audio_frames.saturating_add(1);
                    }
                }
                MediaType::Video => {
                    if let Some(epoch) = self.post_seek_epoch.as_mut() {
                        epoch.dropped_video_frames = epoch.dropped_video_frames.saturating_add(1);
                    }
                    self.diagnostics.dropped_video_frames =
                        self.diagnostics.dropped_video_frames.saturating_add(1);
                }
                _ => {}
            }
            return true;
        }

        let (dropped, complete) = {
            let epoch = self
                .post_seek_epoch
                .as_mut()
                .expect("post-seek epoch checked above");
            let dropped = match kind {
                MediaType::Audio => {
                    epoch.audio_aligned = true;
                    epoch.dropped_audio_frames
                }
                MediaType::Video => {
                    epoch.video_aligned = true;
                    epoch.dropped_video_frames
                }
                _ => 0,
            };
            (dropped, epoch.audio_aligned && epoch.video_aligned)
        };

        log::info!(
            "SanctuaryPlayer: post-seek {} aligned floor={:.3}s first={:.3}s dropped_pre_epoch={}",
            match kind {
                MediaType::Audio => "audio",
                MediaType::Video => "video",
                _ => "track",
            },
            epoch.floor.as_secs_f64(),
            position.as_secs_f64(),
            dropped,
        );
        if complete {
            self.post_seek_epoch = None;
        }
        false
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
                log::info!("SanctuaryPlayer: first decoded video PTS={seconds:.3}s raw={pts}");
            }
            MediaType::Audio if self.first_audio_seconds.is_none() => {
                self.first_audio_seconds = Some(seconds);
                log::info!("SanctuaryPlayer: first decoded audio PTS={seconds:.3}s raw={pts}");
            }
            _ => {}
        }
        if kind == MediaType::Audio && self.audio_anchor_seconds.is_none() {
            self.audio_anchor_seconds = Some(seconds);
            log::info!("SanctuaryPlayer: audio epoch PTS={seconds:.3}s raw={pts}");
        }
        if self.timeline_origin_seconds.is_none() {
            self.timeline_origin_seconds = timeline_origin_seconds(
                self.first_video_seconds,
                self.first_audio_seconds,
                self.audio_stream.is_some(),
            );
            if let Some(origin) = self.timeline_origin_seconds {
                log::info!(
                    "SanctuaryPlayer: media timeline origin={origin:.3}s video_start={:?} audio_start={:?}",
                    self.first_video_seconds,
                    self.first_audio_seconds
                );
            }
        }
    }

    fn sync_audio_origin(&mut self) -> Result<(), String> {
        let (Some(origin), Some(audio_anchor), Some(audio)) = (
            self.timeline_origin_seconds,
            self.audio_anchor_seconds,
            self.audio_output.as_mut(),
        ) else {
            return Ok(());
        };
        let relative = (audio_anchor - origin).max(0.0);
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

    fn forward_buffers_ready(&self) -> bool {
        let video_ready = self.video_queue.len() >= VIDEO_QUEUE_CAP;
        let audio_ready = if self.audio_stream.is_some() {
            self.audio_output.as_ref().is_some_and(|audio| {
                audio.preroll_ready() && audio.queued_samples() >= audio.queue_target_samples()
            })
        } else {
            true
        };
        video_ready && audio_ready
    }

    fn note_or_enter_buffering(&mut self, now: Instant) {
        if !matches!(self.state, PlaybackState::Playing) || !self.first_frame_presented {
            self.starvation_started_at = None;
            return;
        }
        if !self.video_queue.is_empty() {
            self.starvation_started_at = None;
            return;
        }

        let started = *self.starvation_started_at.get_or_insert(now);
        if now.duration_since(started) < BUFFERING_GRACE {
            return;
        }

        log::info!("SanctuaryPlayer: playback -> Buffering");
        self.video_clock.pause(now, self.video_stream.time_base);
        self.update_position_at(now);
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(true)
        {
            self.fail(error);
            return;
        }
        self.starvation_started_at = None;
        self.state = PlaybackState::Buffering;
    }

    fn resume_from_buffering_if_ready(&mut self, now: Instant) {
        if !matches!(self.state, PlaybackState::Buffering) || !self.forward_buffers_ready() {
            return;
        }
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(false)
        {
            self.fail(error);
            return;
        }
        self.video_clock.play(now);
        self.state = PlaybackState::Playing;
        log::info!("SanctuaryPlayer: playback -> Playing after buffering");
    }

    fn fail(&mut self, message: String) {
        log::error!("SanctuaryPlayer: {message}");
        self.starvation_started_at = None;
        if let Some(executor) = self.executor.as_ref() {
            executor.request_abort();
        }
        if let Some(audio) = self.audio_output.as_mut() {
            let _ = audio.set_paused(true);
        }
        let now = Instant::now();
        self.video_clock.pause(now, self.video_stream.time_base);
        self.update_position_at(now);
        self.state = PlaybackState::Error(message);
    }

    fn update_end_state(&mut self) {
        let epoch_finished = self.sink_finished
            || (self.video_eof && (self.audio_stream.is_none() || self.audio_eof));
        if !epoch_finished || !self.video_queue.is_empty() || self.pending_audio_frame.is_some() {
            return;
        }
        if self
            .audio_output
            .as_ref()
            .is_some_and(|audio| audio.queued_samples() != 0)
        {
            return;
        }
        if matches!(
            self.state,
            PlaybackState::Error(_) | PlaybackState::Ended | PlaybackState::Seeking
        ) {
            return;
        }
        if !self.first_frame_presented {
            log::info!(
                "SanctuaryPlayer: playback ended without presenting a video frame; treating as normal end-of-media"
            );
        }
        self.state = PlaybackState::Ended;
    }

    fn frame_position(&self, frame: &FrameLease) -> Option<Duration> {
        self.frame_position_for_kind(MediaType::Video, frame.pts())
    }

    fn video_position_at(&self, now: Instant) -> Option<Duration> {
        let pts = self.video_clock.pts_at(now, self.video_stream.time_base)?;
        self.frame_position_for_kind(MediaType::Video, Some(pts))
    }

    fn update_position_at(&mut self, now: Instant) {
        if matches!(self.state, PlaybackState::Seeking) {
            return;
        }
        if let Some(pts) = self.video_clock.pts_at(now, self.video_stream.time_base)
            && let Some(position) = self.frame_position_for_kind(MediaType::Video, Some(pts))
        {
            self.position = position;
        }

        if let Some(duration) = self.duration
            && self.position >= duration
        {
            self.position = duration;
        }
    }

    fn take_due_frame_at(&mut self, now: Instant) -> Option<FrameLease> {
        self.pump_session();
        self.resume_from_buffering_if_ready(now);
        self.note_or_enter_buffering(now);

        if self
            .video_clock
            .pts_at(now, self.video_stream.time_base)
            .is_none()
        {
            let pts = self.video_queue.front()?.pts()?;
            self.video_clock
                .establish(pts, now, matches!(self.state, PlaybackState::Playing));
            self.update_position_at(now);
        }

        if self.first_frame_presented && !matches!(self.state, PlaybackState::Playing) {
            return None;
        }

        let desired_pts = self.video_clock.pts_at(now, self.video_stream.time_base)?;
        loop {
            let newer_due = self
                .video_queue
                .get(1)
                .and_then(FrameLease::pts)
                .is_some_and(|pts| pts <= desired_pts);
            if !newer_due {
                break;
            }
            self.video_queue.pop_front();
            self.diagnostics.dropped_video_frames =
                self.diagnostics.dropped_video_frames.saturating_add(1);
            self.pump_session();
        }

        let frame_pts = self.video_queue.front()?.pts()?;
        if frame_pts > desired_pts {
            return None;
        }

        let frame = self.video_queue.pop_front()?;
        self.first_frame_presented = true;
        self.diagnostics.presented_video_frames =
            self.diagnostics.presented_video_frames.saturating_add(1);
        if !matches!(self.state, PlaybackState::Playing)
            && let Some(position) = self.frame_position(&frame)
        {
            self.position = position;
        }
        self.pump_session();
        self.note_or_enter_buffering(now);
        Some(frame)
    }

    fn take_due_frame(&mut self) -> Option<FrameLease> {
        self.take_due_frame_at(Instant::now())
    }

    fn next_video_wake_deadline_at(&self, now: Instant) -> Option<Instant> {
        if !matches!(self.state, PlaybackState::Playing) {
            return None;
        }
        if let Some(started) = self.starvation_started_at {
            return Some((started + BUFFERING_GRACE).max(now));
        }
        let frame_pts = self.video_queue.front()?.pts()?;
        let Some(desired_pts) = self.video_clock.pts_at(now, self.video_stream.time_base) else {
            return Some(now);
        };
        if frame_pts <= desired_pts {
            Some(now)
        } else {
            self.video_clock
                .deadline_for(frame_pts, self.video_stream.time_base)
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
            log::info!(
                "SanctuaryPlayer: A/V status state={:?} clock={:.3}s pump={} executor_finished={} sink_finished={} video[q={} front={}s back={}s recv={} present={} drop={}] audio[playing={} preroll={} queued={:.1}ms headroom={:.1}ms submitted_samples={} next_output_pts={:?} underrun_callbacks={} underrun_samples={}]",
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
                audio.submitted_samples(),
                audio.next_output_pts(),
                audio.underrun_callbacks(),
                audio.underrun_samples(),
            );
        } else {
            log::info!(
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

    fn dispatch_seek(
        &mut self,
        target: Duration,
        prior_position: Duration,
        resume_playing: bool,
    ) -> Result<(), String> {
        let origin = self
            .timeline_origin_seconds
            .ok_or_else(|| "cannot seek before media timeline origin is known".to_owned())?;
        let stream = &self.video_stream;
        let raw_seconds = origin + target.as_secs_f64();
        let pts = stream_pts_for_media_position(stream, target, origin)
            .ok_or_else(|| "cannot seek: invalid video time base".to_owned())?;
        let executor = self
            .executor
            .as_ref()
            .ok_or_else(|| "cannot seek without an active OxideAV executor".to_owned())?;
        let generation = executor
            .seek_with_generation(stream.index, pts, stream.time_base)
            .map_err(|error| format!("dispatch OxideAV seek: {error}"))?;

        self.pending_audio_frame = None;
        self.post_seek_epoch = None;
        self.video_queue.clear();
        self.video_clock.reset();
        self.first_frame_presented = false;
        self.starvation_started_at = None;
        self.position = target;
        self.state = PlaybackState::Seeking;
        self.seek_pending = Some(PendingSeek {
            generation,
            requested: target,
            prior_position,
            resume_playing,
            barriers_remaining: usize::from(self.audio_stream.is_some()) + 1,
            landing: None,
            rejected: false,
        });
        log::info!(
            "SanctuaryPlayer: seek begin generation={} media={:.3}s raw={:.3}s resume_playing={}",
            generation,
            target.as_secs_f64(),
            raw_seconds,
            resume_playing,
        );
        Ok(())
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

    fn intends_playing(&self) -> bool {
        matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) || self
            .seek_pending
            .as_ref()
            .is_some_and(|pending| pending.resume_playing)
    }

    fn play(&mut self) {
        if matches!(self.state, PlaybackState::Seeking) {
            if let Some(pending) = self.seek_pending.as_mut() {
                pending.resume_playing = true;
            }
            log::info!("SanctuaryPlayer: playback seek will resume after completion");
            return;
        }
        if !matches!(self.state, PlaybackState::Paused) {
            return;
        }
        log::info!("SanctuaryPlayer: playback -> Playing");
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(false)
        {
            self.fail(error);
            return;
        }
        self.video_clock.play(Instant::now());
        self.state = PlaybackState::Playing;
    }

    fn pause(&mut self) {
        if matches!(self.state, PlaybackState::Seeking) {
            if let Some(pending) = self.seek_pending.as_mut() {
                pending.resume_playing = false;
            }
            log::info!("SanctuaryPlayer: playback seek will remain paused after completion");
            return;
        }
        if !matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) {
            return;
        }
        log::info!("SanctuaryPlayer: playback -> Paused");
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(true)
        {
            self.fail(error);
            return;
        }
        let now = Instant::now();
        self.video_clock.pause(now, self.video_stream.time_base);
        self.update_position_at(now);
        self.starvation_started_at = None;
        self.state = PlaybackState::Paused;
    }

    fn position(&self) -> Duration {
        if !matches!(self.state, PlaybackState::Seeking | PlaybackState::Error(_))
            && let Some(position) = self.video_position_at(Instant::now())
        {
            return self
                .duration
                .map_or(position, |duration| position.min(duration));
        }
        self.position
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }

    fn seek(&mut self, position: Duration) {
        if !self.seek_supported {
            log::info!("SanctuaryPlayer: seek ignored; source rejected seeking earlier");
            return;
        }
        if self.timeline_origin_seconds.is_none() {
            log::info!("SanctuaryPlayer: seek ignored until media timeline origin is known");
            return;
        }
        let target = self
            .duration
            .map_or(position, |duration| position.min(duration));

        // HLS/MPEG-TS seeking can require a new HTTP segment open and an
        // access-point search. Never queue another physical source seek behind
        // one already in progress: repeated keyboard/scrubber input would make
        // the source execute every obsolete intermediate destination before
        // reaching the one the user still wants. Keep only the latest target;
        // relative seeks continue composing from `self.position`, which we move
        // immediately to that visible destination while the active generation
        // finishes.
        if let Some(pending) = self.seek_pending.as_ref() {
            self.queued_seek = Some(target);
            self.position = target;
            log::info!(
                "SanctuaryPlayer: seek coalesced behind generation={} latest={:.3}s",
                pending.generation,
                target.as_secs_f64(),
            );
            return;
        }
        if self.executor.is_none() {
            return;
        }

        let prior_position = self.position;
        let resume_playing = matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        );

        // Freeze the application-owned audio clock before the seek command can
        // move the source. Otherwise a fast source thread could land and start
        // filling post-seek pipeline state while OSS is still consuming the
        // old PCM ring.
        if let Some(audio) = self.audio_output.as_mut()
            && let Err(error) = audio.set_paused(true)
        {
            self.fail(error);
            return;
        }
        self.queued_seek = None;
        if let Err(error) = self.dispatch_seek(target, prior_position, resume_playing) {
            if resume_playing && let Some(audio) = self.audio_output.as_mut() {
                let _ = audio.set_paused(false);
            }
            self.fail(error);
        }
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

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 2.0);
        if let Some(audio) = self.audio_output.as_ref() {
            audio.set_volume(if self.muted { 0.0 } else { self.volume });
        }
    }

    fn available_qualities(&self) -> &[Quality] {
        &self.qualities
    }

    fn quality(&self) -> Option<&Quality> {
        self.qualities.get(self.quality_index)
    }

    fn quality_master_url(&self) -> Option<&Url> {
        Some(&self.master_url)
    }

    fn set_quality(&mut self, quality_id: &str) {
        if let Some(index) = self
            .qualities
            .iter()
            .position(|quality| quality.id == quality_id)
        {
            self.quality_index = index;
        }
    }

    fn update(&mut self, _elapsed: Duration) {
        self.pump_session();
        let now = Instant::now();
        self.resume_from_buffering_if_ready(now);
        self.note_or_enter_buffering(now);
        self.update_position_at(now);
        self.update_end_state();
        self.maybe_log_status();
    }

    fn next_wake_deadline(&self, now: Instant) -> Option<Instant> {
        self.next_video_wake_deadline_at(now)
    }

    fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        self.take_due_frame()
    }

    fn video_color_info(&self) -> Option<VideoColorInfo> {
        self.video_stream.params.video_color
    }

    fn debug_info(&self) -> Vec<DebugInfoSection> {
        let now = Instant::now();
        let state = format!("{:?}", self.state);
        let position = self.position();
        let duration = self
            .duration
            .map(|value| format!("{:.3}s", value.as_secs_f64()))
            .unwrap_or_else(|| "unknown".into());
        let quality = self
            .qualities
            .get(self.quality_index)
            .map(|quality| quality.label.clone())
            .unwrap_or_else(|| "unknown".into());
        let error = match &self.state {
            PlaybackState::Error(message) => message.as_str(),
            _ => "-",
        };

        let executor = match self.executor.as_ref() {
            None => "none".to_owned(),
            Some(executor) if executor.has_finished() => "finished".to_owned(),
            Some(_) => "running".to_owned(),
        };
        let transport_state = match self.state {
            PlaybackState::Loading => "opening",
            PlaybackState::Buffering => "waiting for media",
            PlaybackState::Error(_) => "terminal error",
            PlaybackState::Ended => "ended",
            _ if self.sink_finished => "sink finished",
            _ => "active",
        };
        let hls_host = self
            .quality_urls
            .get(self.quality_index)
            .and_then(Url::host_str)
            .unwrap_or("unknown")
            .to_owned();

        let video = &self.video_stream;
        let video_front = self
            .video_queue
            .front()
            .and_then(FrameLease::pts)
            .map(|pts| format!("{pts} ({:.3}s)", video.time_base.seconds_of(pts)))
            .unwrap_or_else(|| "-".into());
        let video_back = self
            .video_queue
            .back()
            .and_then(FrameLease::pts)
            .map(|pts| format!("{pts} ({:.3}s)", video.time_base.seconds_of(pts)))
            .unwrap_or_else(|| "-".into());
        let video_clock = self
            .video_clock
            .pts_at(now, video.time_base)
            .map(|pts| format!("{pts} ({:.3}s)", video.time_base.seconds_of(pts)))
            .unwrap_or_else(|| "-".into());
        let video_dims = match (video.params.width, video.params.height) {
            (Some(width), Some(height)) => format!("{width}x{height}"),
            _ => "unknown".into(),
        };

        let audio_rows = if let Some(audio_stream) = self.audio_stream.as_ref() {
            let output = self.audio_output.as_ref();
            vec![
                ("codec".into(), audio_stream.params.codec_id.to_string()),
                (
                    "OxideAV decoder".into(),
                    self.audio_decoder
                        .as_ref()
                        .map(|decoder| decoder.implementation.clone())
                        .unwrap_or_else(|| "unknown".into()),
                ),
                (
                    "decoder acceleration".into(),
                    self.audio_decoder
                        .as_ref()
                        .map(|decoder| {
                            if decoder.hardware_accelerated {
                                "hardware"
                            } else {
                                "software"
                            }
                            .to_owned()
                        })
                        .unwrap_or_else(|| "unknown".into()),
                ),
                (
                    "time base".into(),
                    format!(
                        "{}/{}",
                        audio_stream.time_base.num(),
                        audio_stream.time_base.den()
                    ),
                ),
                (
                    "sample rate".into(),
                    audio_stream
                        .params
                        .sample_rate
                        .map(|value| format!("{value} Hz"))
                        .unwrap_or_else(|| "unknown".into()),
                ),
                (
                    "channels".into(),
                    audio_stream
                        .params
                        .resolved_channels()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                ),
                (
                    "sample format".into(),
                    audio_stream
                        .params
                        .sample_format
                        .map(|value| format!("{value:?}"))
                        .unwrap_or_else(|| "unknown".into()),
                ),
                (
                    "decoded frames".into(),
                    self.diagnostics.received_audio_frames.to_string(),
                ),
                (
                    "pending frame".into(),
                    self.pending_audio_frame.is_some().to_string(),
                ),
                (
                    "output backend".into(),
                    output
                        .map(|audio| audio.backend_name().to_owned())
                        .unwrap_or_else(|| "not open".into()),
                ),
                (
                    "device rate".into(),
                    output
                        .map(|audio| format!("{} Hz", audio.device_rate()))
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "playing".into(),
                    output
                        .map(|audio| audio.is_playing().to_string())
                        .unwrap_or_else(|| "false".into()),
                ),
                (
                    "preroll ready".into(),
                    output
                        .map(|audio| audio.preroll_ready().to_string())
                        .unwrap_or_else(|| "false".into()),
                ),
                (
                    "ring queued".into(),
                    output
                        .map(|audio| {
                            format!(
                                "{} samples / {:.1} ms",
                                audio.queued_samples(),
                                audio.queued_duration().as_secs_f64() * 1000.0
                            )
                        })
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "ring target".into(),
                    output
                        .map(|audio| format!("{} samples", audio.queue_target_samples()))
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "ring headroom".into(),
                    output
                        .map(|audio| {
                            format!(
                                "{} samples / {:.1} ms",
                                audio.headroom_samples(),
                                audio.headroom_duration().as_secs_f64() * 1000.0
                            )
                        })
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "submitted samples".into(),
                    output
                        .map(|audio| audio.submitted_samples().to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "next output PTS".into(),
                    output
                        .and_then(AudioOutput::next_output_pts)
                        .map(|pts| pts.to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "media origin".into(),
                    output
                        .and_then(AudioOutput::media_origin)
                        .map(|origin| format!("{:.3}s", origin.as_secs_f64()))
                        .unwrap_or_else(|| "-".into()),
                ),
                (
                    "underruns".into(),
                    output
                        .map(|audio| {
                            format!(
                                "{} callbacks / {} samples",
                                audio.underrun_callbacks(),
                                audio.underrun_samples()
                            )
                        })
                        .unwrap_or_else(|| "-".into()),
                ),
            ]
        } else {
            vec![("stream".into(), "none".into())]
        };

        let av_offset = match (self.first_video_seconds, self.first_audio_seconds) {
            (Some(video), Some(audio)) => format!("{:+.3}s audio-video", audio - video),
            _ => "-".into(),
        };
        let post_seek = self
            .post_seek_epoch
            .map(|epoch| {
                format!(
                    "floor={:.3}s audio_aligned={} video_aligned={} drops={}/{}",
                    epoch.floor.as_secs_f64(),
                    epoch.audio_aligned,
                    epoch.video_aligned,
                    epoch.dropped_audio_frames,
                    epoch.dropped_video_frames
                )
            })
            .unwrap_or_else(|| "none".into());

        vec![
            DebugInfoSection::new(
                "Playback",
                vec![
                    ("state".into(), state),
                    ("position".into(), format!("{:.3}s", position.as_secs_f64())),
                    ("duration".into(), duration),
                    ("rate".into(), format!("{:.3}x", self.rate)),
                    ("quality".into(), quality),
                    ("decode mode".into(), self.decode_mode.to_string()),
                    ("muted".into(), self.muted.to_string()),
                    ("error".into(), error.into()),
                ],
            ),
            DebugInfoSection::new(
                "Transport / pipeline",
                vec![
                    ("network / stream".into(), transport_state.into()),
                    ("HLS host".into(), hls_host),
                    ("executor".into(), executor),
                    ("sink finished".into(), self.sink_finished.to_string()),
                    (
                        "session channel caps".into(),
                        format!(
                            "control={} audio={} video={}",
                            SESSION_CHANNEL_CAP, SESSION_CHANNEL_CAP, SESSION_CHANNEL_CAP
                        ),
                    ),
                    (
                        "compressed packet cap".into(),
                        format!("{PLAYBACK_PACKET_CHANNEL_CAP} / track"),
                    ),
                    (
                        "starvation grace".into(),
                        self.starvation_started_at
                            .map(|started| {
                                format!(
                                    "{:.0} ms",
                                    now.duration_since(started).as_secs_f64() * 1000.0
                                )
                            })
                            .unwrap_or_else(|| "inactive".into()),
                    ),
                ],
            ),
            DebugInfoSection::new(
                "Video",
                vec![
                    ("codec".into(), video.params.codec_id.to_string()),
                    (
                        "OxideAV decoder".into(),
                        self.video_decoder
                            .as_ref()
                            .map(|decoder| decoder.implementation.clone())
                            .unwrap_or_else(|| "unknown".into()),
                    ),
                    (
                        "decoder acceleration".into(),
                        self.video_decoder
                            .as_ref()
                            .map(|decoder| {
                                if decoder.hardware_accelerated {
                                    "hardware"
                                } else {
                                    "software"
                                }
                                .to_owned()
                            })
                            .unwrap_or_else(|| "unknown".into()),
                    ),
                    ("coded size".into(), video_dims),
                    (
                        "time base".into(),
                        format!("{}/{}", video.time_base.num(), video.time_base.den()),
                    ),
                    (
                        "start PTS".into(),
                        video
                            .start_time
                            .map(|pts| pts.to_string())
                            .unwrap_or_else(|| "-".into()),
                    ),
                    (
                        "colour".into(),
                        video
                            .params
                            .video_color
                            .map(|value| format!("{value:?}"))
                            .unwrap_or_else(|| "unknown".into()),
                    ),
                    (
                        "decoded queue".into(),
                        format!("{} / {}", self.video_queue.len(), VIDEO_QUEUE_CAP),
                    ),
                    ("queue front".into(), video_front),
                    ("queue back".into(), video_back),
                    ("video clock".into(), video_clock),
                    (
                        "first frame presented".into(),
                        self.first_frame_presented.to_string(),
                    ),
                    (
                        "frames".into(),
                        format!(
                            "recv={} present={} drop={}",
                            self.diagnostics.received_video_frames,
                            self.diagnostics.presented_video_frames,
                            self.diagnostics.dropped_video_frames
                        ),
                    ),
                ],
            ),
            DebugInfoSection::new("Audio", audio_rows),
            DebugInfoSection::new(
                "Timeline / seek",
                vec![
                    (
                        "timeline origin".into(),
                        self.timeline_origin_seconds
                            .map(|value| format!("{value:.3}s"))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    (
                        "first video".into(),
                        self.first_video_seconds
                            .map(|value| format!("{value:.3}s"))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    (
                        "first audio".into(),
                        self.first_audio_seconds
                            .map(|value| format!("{value:.3}s"))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    ("first A/V offset".into(), av_offset),
                    (
                        "audio anchor".into(),
                        self.audio_anchor_seconds
                            .map(|value| format!("{value:.3}s"))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    (
                        "seek pending".into(),
                        self.seek_pending
                            .map(|pending| {
                                format!(
                                    "gen={} requested={:.3}s barriers={} resume={}",
                                    pending.generation,
                                    pending.requested.as_secs_f64(),
                                    pending.barriers_remaining,
                                    pending.resume_playing
                                )
                            })
                            .unwrap_or_else(|| "none".into()),
                    ),
                    ("post-seek epoch".into(), post_seek),
                    ("seek supported".into(), self.seek_supported.to_string()),
                ],
            ),
        ]
    }

    fn debug_graph(&self) -> DebugGraph {
        let sections = self.debug_info();
        let rows_for = |title: &str| {
            sections
                .iter()
                .find(|section| section.title == title)
                .map(|section| section.rows.clone())
                .unwrap_or_default()
        };
        let row_value = |title: &str, name: &str| {
            sections
                .iter()
                .find(|section| section.title == title)
                .and_then(|section| section.rows.iter().find(|(label, _)| label == name))
                .map(|(_, value)| value.clone())
                .unwrap_or_else(|| "-".into())
        };

        let mut graph = DebugGraph::default();
        let transport_rows = rows_for("Transport / pipeline");
        let packet_queue_depths = self
            .executor
            .as_ref()
            .map(ExecutorHandle::pipeline_packet_queue_depths)
            .unwrap_or_default();
        let topology = self
            .executor
            .as_ref()
            .map(ExecutorHandle::pipeline_topology);
        let source_summary = topology
            .and_then(|topology| topology.tracks.first())
            .map(|track| format!("{:?}", track.source_shape))
            .unwrap_or_else(|| "not open".into());
        let mut source_rows = vec![
            (
                "HLS host".into(),
                row_value("Transport / pipeline", "HLS host"),
            ),
            (
                "network / stream".into(),
                row_value("Transport / pipeline", "network / stream"),
            ),
            ("source shape".into(), source_summary.clone()),
        ];
        if let Some(topology) = topology {
            source_rows.push(("executor output".into(), topology.output_name.clone()));
        }
        graph.nodes.push(DebugNode::new(
            "media-source",
            "HLS / OxideAV source",
            source_summary,
            DebugGraphLane::Shared,
            0,
            source_rows,
        ));

        if let Some(topology) = topology {
            for (track_index, track) in topology.tracks.iter().enumerate() {
                let (lane, lane_name) = match track.media_type {
                    MediaType::Video => (DebugGraphLane::Video, "video"),
                    MediaType::Audio => (DebugGraphLane::Audio, "audio"),
                    _ => continue,
                };
                let packet_depth = packet_queue_depths.get(track_index).copied().unwrap_or(0);
                let queue_id = format!("oxideav-{lane_name}-packet-queue");
                graph.nodes.push(DebugNode::new(
                    queue_id.clone(),
                    format!("OxideAV {lane_name} packet queue"),
                    format!("{packet_depth} / {}", topology.packet_channel_capacity),
                    lane,
                    1,
                    vec![
                        ("source stream".into(), track.source_stream.to_string()),
                        ("codec".into(), track.codec_id.to_string()),
                        (
                            "capacity".into(),
                            topology.packet_channel_capacity.to_string(),
                        ),
                        ("live depth".into(), packet_depth.to_string()),
                        ("source shape".into(), format!("{:?}", track.source_shape)),
                    ],
                ));
                graph.edges.push(DebugEdge::flow(
                    "media-source",
                    queue_id.clone(),
                    "compressed packets",
                ));

                let mut previous = queue_id;
                let mut next_column = 2_u8;
                for (stage_index, stage) in track.stages.iter().enumerate() {
                    let stage_id = format!("oxideav-{lane_name}-stage-{stage_index}");
                    let (title, summary, rows) = match stage {
                        PipelineStageInfo::Copy => (
                            "OxideAV stream copy".to_owned(),
                            track.codec_id.to_string(),
                            vec![("codec".into(), track.codec_id.to_string())],
                        ),
                        PipelineStageInfo::Decode { capabilities } => {
                            let acceleration = if capabilities.hardware_accelerated {
                                "hardware"
                            } else {
                                "software"
                            };
                            (
                                capabilities.implementation.clone(),
                                format!("{} · {acceleration}", track.codec_id),
                                vec![
                                    ("stage".into(), "decode".into()),
                                    ("codec".into(), track.codec_id.to_string()),
                                    ("implementation".into(), capabilities.implementation.clone()),
                                    ("acceleration".into(), acceleration.into()),
                                ],
                            )
                        }
                        PipelineStageInfo::Filter { name } => (
                            format!("OxideAV filter: {name}"),
                            "frame filter".into(),
                            vec![
                                ("stage".into(), "filter".into()),
                                ("implementation".into(), name.clone()),
                            ],
                        ),
                        PipelineStageInfo::PixelFormatConvert { target } => (
                            "OxideAV pixel conversion".into(),
                            format!("{target:?}"),
                            vec![
                                ("stage".into(), "pixel conversion".into()),
                                ("target".into(), format!("{target:?}")),
                            ],
                        ),
                        PipelineStageInfo::Encode { capabilities } => {
                            let acceleration = if capabilities.hardware_accelerated {
                                "hardware"
                            } else {
                                "software"
                            };
                            (
                                capabilities.implementation.clone(),
                                format!("encoder · {acceleration}"),
                                vec![
                                    ("stage".into(), "encode".into()),
                                    ("implementation".into(), capabilities.implementation.clone()),
                                    ("acceleration".into(), acceleration.into()),
                                ],
                            )
                        }
                    };
                    graph.nodes.push(DebugNode::new(
                        stage_id.clone(),
                        title,
                        summary,
                        lane,
                        next_column,
                        rows,
                    ));
                    graph
                        .edges
                        .push(DebugEdge::flow(previous, stage_id.clone(), ""));
                    previous = stage_id;
                    next_column = next_column.saturating_add(1);
                }

                let session_depth = self.channel_depths.current(track.media_type);
                let session_id = format!("sanctuary-{lane_name}-session-channel");
                graph.nodes.push(DebugNode::new(
                    session_id.clone(),
                    format!("Sanctuary {lane_name} TrackSink / session channel"),
                    format!("{session_depth} / {SESSION_CHANNEL_CAP}"),
                    lane,
                    next_column,
                    vec![
                        ("capacity".into(), SESSION_CHANNEL_CAP.to_string()),
                        ("live depth".into(), session_depth.to_string()),
                        ("track".into(), track_index.to_string()),
                    ],
                ));
                graph.edges.push(DebugEdge::flow(
                    previous,
                    session_id.clone(),
                    "decoded frames",
                ));

                match track.media_type {
                    MediaType::Video => {
                        graph.nodes.push(DebugNode::new(
                            "video-lookahead",
                            "Decoded video lookahead",
                            format!("{} / {}", self.video_queue.len(), VIDEO_QUEUE_CAP),
                            DebugGraphLane::Video,
                            next_column.saturating_add(1),
                            rows_for("Video"),
                        ));
                        graph.edges.push(DebugEdge::flow(
                            session_id,
                            "video-lookahead",
                            "FrameLease",
                        ));
                    }
                    MediaType::Audio => {
                        graph.nodes.push(DebugNode::new(
                            "audio-convert",
                            "AudioOutput PCM conversion",
                            "decoded audio → interleaved f32",
                            DebugGraphLane::Audio,
                            next_column.saturating_add(1),
                            vec![
                                ("conversion".into(), "decode_to_f32".into()),
                                ("layout".into(), "interleaved f32".into()),
                                ("source format".into(), row_value("Audio", "sample format")),
                                ("source rate".into(), row_value("Audio", "sample rate")),
                                ("channels".into(), row_value("Audio", "channels")),
                            ],
                        ));
                        graph.edges.push(DebugEdge::flow(
                            session_id,
                            "audio-convert",
                            "decoded frames",
                        ));
                        graph.nodes.push(DebugNode::new(
                            "audio-pcm-ring",
                            "PcmTimeline ring",
                            row_value("Audio", "ring queued"),
                            DebugGraphLane::Audio,
                            next_column.saturating_add(2),
                            vec![
                                ("queued".into(), row_value("Audio", "ring queued")),
                                ("target".into(), row_value("Audio", "ring target")),
                                ("headroom".into(), row_value("Audio", "ring headroom")),
                                (
                                    "next output PTS".into(),
                                    row_value("Audio", "next output PTS"),
                                ),
                                ("media origin".into(), row_value("Audio", "media origin")),
                                (
                                    "pending decoded frame".into(),
                                    row_value("Audio", "pending frame"),
                                ),
                            ],
                        ));
                        graph.edges.push(DebugEdge::flow(
                            "audio-convert",
                            "audio-pcm-ring",
                            "f32 PCM",
                        ));
                        let backend = row_value("Audio", "output backend");
                        graph.nodes.push(DebugNode::new(
                            "audio-device",
                            format!("sysaudio {backend}"),
                            format!(
                                "{} · {}",
                                row_value("Audio", "playing"),
                                row_value("Audio", "device rate")
                            ),
                            DebugGraphLane::Audio,
                            next_column.saturating_add(3),
                            vec![
                                ("backend".into(), backend),
                                ("device rate".into(), row_value("Audio", "device rate")),
                                ("playing".into(), row_value("Audio", "playing")),
                                (
                                    "submitted samples".into(),
                                    row_value("Audio", "submitted samples"),
                                ),
                                ("underruns".into(), row_value("Audio", "underruns")),
                            ],
                        ));
                        graph.edges.push(DebugEdge::flow(
                            "audio-pcm-ring",
                            "audio-device",
                            "device callback",
                        ));
                    }
                    _ => {}
                }
            }
        }

        graph.nodes.push(DebugNode::new(
            "playback-control",
            "Playback / control",
            row_value("Playback", "state"),
            DebugGraphLane::Shared,
            2,
            rows_for("Playback"),
        ));
        graph.nodes.push(DebugNode::new(
            "timeline",
            "Timeline / seek",
            row_value("Timeline / seek", "timeline origin"),
            DebugGraphLane::Shared,
            3,
            rows_for("Timeline / seek"),
        ));
        graph.edges.push(DebugEdge::relationship(
            "timeline",
            "video-lookahead",
            "video clock",
        ));
        graph.edges.push(DebugEdge::relationship(
            "timeline",
            "audio-pcm-ring",
            "audio PTS",
        ));

        if !transport_rows.is_empty() {
            graph.nodes.push(DebugNode::new(
                "pipeline-control",
                "OxideAV executor",
                row_value("Transport / pipeline", "executor"),
                DebugGraphLane::Shared,
                1,
                transport_rows,
            ));
            graph.edges.push(DebugEdge::relationship(
                "pipeline-control",
                "media-source",
                "owns source pump",
            ));
        }

        graph
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

fn select_initial_quality_index(
    qualities: &[Quality],
    fallback_index: usize,
    initial_qualities: &str,
) -> usize {
    for wanted in initial_qualities
        .split([',', ';'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        if let Some(index) = qualities
            .iter()
            .position(|quality| quality.id == wanted || quality.label == wanted)
        {
            return index;
        }
    }
    fallback_index
}

fn inspect_hls_qualities(
    master_url: &Url,
    cancellation: &CancellationToken,
) -> Result<HlsQualitySet, String> {
    let inspected = oxideav_hls::inspect_hls_cancellable(&hls_uri(master_url), cancellation)
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
    let preferred_height = variants[preferred_variant].height;

    let video_variants: Vec<HlsVariant> = variants
        .into_iter()
        .filter(|variant| {
            variant.width.is_some_and(|width| width > 0)
                && variant.height.is_some_and(|height| height > 0)
                && variant_uses_supported_video_codec(variant)
        })
        .collect();
    if video_variants.is_empty() {
        return Err("HLS master contains no video variants with a supported codec".into());
    }

    let preferred_index = video_variants
        .iter()
        .position(|variant| variant.url == preferred_url)
        .or_else(|| {
            video_variants
                .iter()
                .enumerate()
                .filter(|(_, variant)| {
                    preferred_height.is_none_or(|height| variant.height.unwrap_or(0) <= height)
                })
                .max_by_key(|(_, variant)| (variant.height.unwrap_or(0), variant.bandwidth))
                .map(|(index, _)| index)
        })
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
        log::info!(
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

fn variant_uses_supported_video_codec(variant: &HlsVariant) -> bool {
    variant.codecs.as_deref().is_none_or(|codecs| {
        codecs
            .split(',')
            .any(|codec| matches!(codec.trim().split('.').next(), Some("avc1" | "avc3")))
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
        DecodeMode::Auto => {
            // Hardware implementations advertise better intrinsic priorities than
            // software. Android ranks direct MediaCodec first, then MediaCodec
            // readback, then h264_sw; FreeBSD ranks VDPAU before h264_sw; Windows
            // ranks Vulkan direct first, then Vulkan readback, then h264_sw. Factory
            // failures therefore walk the same quality order without making the
            // user's automatic request strict.
            CodecPreferences::default()
        }
        DecodeMode::Cpu => CodecPreferences {
            no_hardware: true,
            ..Default::default()
        },
        DecodeMode::VulkanReadback => CodecPreferences {
            prefer: vec!["h264_vulkan".into()],
            exclude: vec!["h264_vulkan_direct".into(), "h264_sw".into()],
            boost: 100,
            ..Default::default()
        },
        DecodeMode::VulkanDirect => CodecPreferences {
            prefer: vec!["h264_vulkan_direct".into()],
            exclude: vec!["h264_vulkan".into(), "h264_sw".into()],
            boost: 100,
            ..Default::default()
        },
        DecodeMode::MediaCodecDirect => CodecPreferences {
            prefer: vec!["h264_mediacodec_direct".into()],
            exclude: vec!["h264_mediacodec_readback".into(), "h264_sw".into()],
            boost: 100,
            ..Default::default()
        },
        DecodeMode::MediaCodecReadback => CodecPreferences {
            prefer: vec!["h264_mediacodec_readback".into()],
            exclude: vec!["h264_mediacodec_direct".into(), "h264_sw".into()],
            boost: 100,
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

    let video_ready = input.video_queue_len >= VIDEO_QUEUE_CAP;
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

fn audio_stream_is_authoritative(stream: &StreamInfo) -> bool {
    stream.params.sample_rate.is_some_and(|rate| rate > 0)
        && stream
            .params
            .resolved_channels()
            .is_some_and(|channels| channels > 0)
        && stream.params.sample_format.is_some()
}

fn timeline_origin_seconds(
    first_video_seconds: Option<f64>,
    first_audio_seconds: Option<f64>,
    has_audio: bool,
) -> Option<f64> {
    match (first_video_seconds, first_audio_seconds, has_audio) {
        (Some(video), Some(audio), true) => Some(video.min(audio)),
        (Some(video), _, false) => Some(video),
        _ => None,
    }
}

fn stream_pts_for_media_position(
    stream: &StreamInfo,
    position: Duration,
    origin_seconds: f64,
) -> Option<i64> {
    let tick_seconds = stream.time_base.as_rational().as_f64();
    if !tick_seconds.is_finite() || tick_seconds <= 0.0 || !origin_seconds.is_finite() {
        return None;
    }
    let raw_seconds = origin_seconds + position.as_secs_f64();
    raw_seconds
        .is_finite()
        .then(|| (raw_seconds / tick_seconds).round() as i64)
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
    use ::oxideav::core::{AudioFrame, CodecId, CodecParameters, SampleFormat, VideoFrame};

    use super::*;

    #[test]
    #[ignore = "requires live YouTube requests"]
    fn probe_live_youtube_playback_open_with_map() {
        let source = VideoSource::parse("https://www.youtube.com/watch?v=TNHNaHOBYG8&t=389s")
            .expect("source");
        let manifest = crate::youtube::resolve_vod_m3u8(&source.id).expect("manifest");
        let mut playback = OxidePlayback::open(
            source,
            manifest,
            "auto",
            DecodeMode::Cpu,
            true,
            PlaybackWake::noop(),
            CancellationToken::new(),
        )
        .expect("playback open");
        println!(
            "YouTube opened: video={:?}, audio={:?}, duration={:?}",
            playback.video_stream.params.codec_id,
            playback.audio_stream.as_ref().map(|s| &s.params.codec_id),
            playback.duration
        );
        playback.seek(Duration::from_secs(389));
        playback.play();
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            playback.update(Duration::from_millis(10));
            if !matches!(playback.state(), PlaybackState::Seeking)
                && playback.take_video_frame_lease().is_some()
            {
                break;
            }
            if let PlaybackState::Error(error) = playback.state() {
                panic!("YouTube seek failed: {error}");
            }
            assert!(
                Instant::now() < deadline,
                "no frame after YouTube seek: state={:?} pending={:?} position={:?} queue={} sink_finished={} received_video={} dropped_video={} executor_finished={}",
                playback.state,
                playback.seek_pending,
                playback.position,
                playback.video_queue.len(),
                playback.sink_finished,
                playback.diagnostics.received_video_frames,
                playback.diagnostics.dropped_video_frames,
                playback
                    .executor
                    .as_ref()
                    .is_none_or(ExecutorHandle::has_finished)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("YouTube seek position={:?}", playback.position());
    }

    #[test]
    #[ignore = "requires live YouTube requests"]
    fn probe_live_youtube_hls_source_seek() {
        enable_http_range_probe();
        let manifest = crate::youtube::resolve_vod_m3u8("TNHNaHOBYG8").expect("manifest");
        let HlsPlaylistInfo::Master { variants, .. } =
            oxideav_hls::inspect_hls(&hls_uri(&manifest)).expect("inspect")
        else {
            panic!("expected master playlist")
        };
        let video = variants
            .iter()
            .find(|variant| {
                variant.height == Some(720)
                    && variant
                        .codecs
                        .as_deref()
                        .is_some_and(|codecs| codecs.starts_with("avc1."))
            })
            .expect("720p H.264 rendition");
        let mut source = oxideav_hls::open_hls(&hls_uri(&video.url)).expect("source");
        let stream = source.streams()[0].clone();
        println!(
            "HLS stream start={:?} base={:?}",
            stream.start_time, stream.time_base
        );
        let ticks = (389.0 / stream.time_base.as_rational().as_f64()).round() as i64;
        let target = stream.start_time.unwrap_or(0) + ticks;
        println!("HLS seek target={target}");
        let landed = source.seek_to(stream.index, target).expect("seek");
        println!("HLS seek landed={landed}");
        let packet = source.next_packet().expect("packet after seek");
        println!("HLS packet pts={:?}", packet.pts);
    }

    #[test]
    #[ignore = "requires live YouTube requests"]
    fn probe_live_youtube_segment_http_open() {
        let source = VideoSource::parse("https://www.youtube.com/watch?v=TNHNaHOBYG8&t=389s")
            .expect("source");
        let manifest = crate::youtube::resolve_vod_m3u8(&source.id).expect("manifest");
        let quality_set =
            inspect_hls_qualities(&manifest, &CancellationToken::new()).expect("qualities");
        let variant = &quality_set.urls[quality_set.preferred_index];
        let playlist = ureq::get(variant.as_str())
            .call()
            .expect("playlist")
            .body_mut()
            .read_to_string()
            .expect("playlist body");
        let segment_ref = playlist
            .lines()
            .find(|line| !line.is_empty() && !line.starts_with('#'))
            .expect("segment reference");
        let segment = variant.join(segment_ref).expect("segment URL");
        let config = oxideav_http::HttpConfig::builder()
            .range_probe(true)
            .build();
        let mut src = oxideav_http::HttpSource::open_with_config(segment.as_str(), &config)
            .expect("segment open");
        println!("YouTube segment length={}", src.len());
        let mut first = [0u8; 16];
        use std::io::{Read as _, Seek as _, SeekFrom};
        src.read_exact(&mut first).expect("read segment");
        src.seek(SeekFrom::Start(0)).expect("rewind segment");
        let mut again = [0u8; 16];
        src.read_exact(&mut again).expect("reread segment");
        assert_eq!(first, again);
    }

    struct TestSessionSenders {
        _control_tx: SyncSender<SessionMsg>,
        audio_tx: SyncSender<SessionMsg>,
        _video_tx: SyncSender<SessionMsg>,
    }

    fn clock_test_playback() -> (OxidePlayback, TestSessionSenders) {
        let (control_tx, control_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        let (audio_tx, audio_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        let (video_tx, video_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
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
                master_url: Url::parse("https://example.test/master.m3u8").unwrap(),
                decode_mode: DecodeMode::Cpu,
                muted: false,
                volume: 1.0,
                control_rx,
                audio_rx,
                video_rx,
                channel_depths: SessionChannelDepths::default(),
                executor: None,
                video_stream,
                audio_stream: None,
                video_decoder: None,
                audio_decoder: None,
                audio_output: None,
                pending_audio_frame: None,
                video_queue: VecDeque::new(),
                video_clock: VideoClock {
                    origin: Some((90_000, Instant::now())),
                    frozen_pts: None,
                },
                timeline_origin_seconds: Some(0.0),
                first_video_seconds: Some(0.0),
                first_audio_seconds: None,
                audio_anchor_seconds: None,
                first_frame_presented: true,
                starvation_started_at: None,
                sink_finished: false,
                video_eof: false,
                audio_eof: false,
                seek_pending: None,
                post_seek_epoch: None,
                queued_seek: None,
                seek_supported: true,
                diagnostics: PlaybackDiagnostics::new(),
            },
            TestSessionSenders {
                _control_tx: control_tx,
                audio_tx,
                _video_tx: video_tx,
            },
        )
    }

    #[test]
    fn retained_eof_enters_ended_without_executor_finish() {
        let (mut playback, _senders) = clock_test_playback();
        playback.video_eof = true;

        playback.update_end_state();

        assert!(matches!(playback.state, PlaybackState::Ended));
        assert!(!playback.sink_finished);
    }

    #[test]
    fn retained_eof_does_not_override_seek_in_progress() {
        let (mut playback, _senders) = clock_test_playback();
        playback.video_eof = true;
        playback.state = PlaybackState::Seeking;

        playback.update_end_state();

        assert!(matches!(playback.state, PlaybackState::Seeking));
    }

    fn audio_frame_lease(samples: usize, pts: i64) -> FrameLease {
        let mut bytes = Vec::with_capacity(samples * 2 * 4);
        for _ in 0..samples {
            bytes.extend_from_slice(&0.25f32.to_le_bytes());
            bytes.extend_from_slice(&(-0.25f32).to_le_bytes());
        }
        FrameLease::from_frame(Frame::Audio(AudioFrame {
            samples: samples as u32,
            pts: Some(pts),
            data: vec![bytes],
        }))
    }

    fn video_frame_lease(pts: Option<i64>) -> FrameLease {
        FrameLease::from_frame(Frame::Video(VideoFrame {
            pts,
            planes: Vec::new(),
        }))
    }

    #[test]
    fn authoritative_audio_update_opens_output_at_decoded_sample_rate() {
        let (mut playback, _tx) = clock_test_playback();
        playback.muted = true;
        playback.audio_stream = Some(StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(6_300_000),
            params: CodecParameters::audio(CodecId::new("aac")),
        });
        assert!(playback.audio_output.is_none());
        assert_eq!(playback.pump_block_reason(), None);

        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::S16);
        let update = StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(6_300_000),
            params,
        };

        playback
            .handle_stream_update_with(update, |stream, muted| {
                let driver = oxideav_sysaudio::driver_by_name("mock")
                    .expect("mock sysaudio driver available");
                AudioOutput::open_with_driver(driver, &stream.params, stream.time_base, muted)
            })
            .unwrap();

        let stream = playback.audio_stream.as_ref().unwrap();
        assert_eq!(stream.params.sample_rate, Some(48_000));
        assert_eq!(stream.params.channels, Some(2));
        let output = playback.audio_output.as_ref().unwrap();
        assert_eq!(output.queue_target_samples(), 24_000);
    }

    #[test]
    fn deferred_audio_frame_stays_head_of_line_on_audio_channel_until_ring_has_space() {
        let (mut playback, senders) = clock_test_playback();
        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::F32);
        let audio_stream = StreamInfo {
            index: 1,
            time_base: TimeBase::AUDIO_48K,
            duration: None,
            start_time: Some(0),
            params: params.clone(),
        };
        let driver =
            oxideav_sysaudio::driver_by_name("mock").expect("mock sysaudio driver available");
        let output =
            AudioOutput::open_with_driver(driver, &params, TimeBase::AUDIO_48K, false).unwrap();
        playback.audio_stream = Some(audio_stream);
        playback.audio_output = Some(output);
        playback.first_audio_seconds = None;
        playback.audio_anchor_seconds = None;

        playback
            .handle_session_message(SessionMsg::Frame {
                kind: MediaType::Audio,
                frame: audio_frame_lease(2_400, 0),
            })
            .unwrap();
        assert!(playback.pending_audio_frame.is_none());

        playback
            .handle_session_message(SessionMsg::Frame {
                kind: MediaType::Audio,
                frame: audio_frame_lease(32, 500_000),
            })
            .unwrap();
        assert!(playback.pending_audio_frame.is_some());
        assert_eq!(playback.pump_block_reason(), Some("audio-frame-pending"));

        senders
            .audio_tx
            .send(SessionMsg::Frame {
                kind: MediaType::Audio,
                frame: audio_frame_lease(32, 600_000),
            })
            .unwrap();

        playback.pump_session();
        assert!(playback.pending_audio_frame.is_some());
        assert!(matches!(
            playback.audio_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Audio,
                ..
            })
        ));
    }

    #[test]
    fn sustained_video_starvation_enters_buffering_and_freezes_clock() {
        let (mut playback, _senders) = clock_test_playback();
        let start = Instant::now();
        playback.video_clock.establish(90_000, start, true);
        playback.video_queue.clear();

        playback.note_or_enter_buffering(start);
        assert_eq!(playback.state, PlaybackState::Playing);
        assert_eq!(
            playback.next_video_wake_deadline_at(start),
            Some(start + BUFFERING_GRACE)
        );

        let buffering_at = start + BUFFERING_GRACE;
        playback.note_or_enter_buffering(buffering_at);

        assert_eq!(playback.state, PlaybackState::Buffering);
        assert_eq!(playback.video_clock.frozen_pts, Some(112_500));
        let frozen_position = playback.position();
        assert_eq!(playback.position(), frozen_position);
    }

    #[test]
    fn buffering_resumes_only_after_forward_video_buffer_refills() {
        let (mut playback, _senders) = clock_test_playback();
        let now = Instant::now();
        playback.state = PlaybackState::Buffering;
        playback.video_clock.establish(90_000, now, false);
        playback.video_queue.clear();

        playback.resume_from_buffering_if_ready(now);
        assert_eq!(playback.state, PlaybackState::Buffering);

        playback
            .video_queue
            .push_back(video_frame_lease(Some(90_000)));
        playback
            .video_queue
            .push_back(video_frame_lease(Some(91_500)));
        playback.resume_from_buffering_if_ready(now);

        assert_eq!(playback.state, PlaybackState::Playing);
        assert!(playback.video_clock.origin.is_some());
        assert!(playback.video_clock.frozen_pts.is_none());
    }

    #[test]
    fn pausing_while_buffering_becomes_paused() {
        let (mut playback, _senders) = clock_test_playback();
        playback.state = PlaybackState::Buffering;
        playback
            .video_clock
            .establish(90_000, Instant::now(), false);

        playback.pause();

        assert_eq!(playback.state, PlaybackState::Paused);
    }

    #[test]
    fn debug_info_exposes_pipeline_video_audio_and_timeline_sections() {
        let (mut playback, _senders) = clock_test_playback();
        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::F32);
        playback.audio_stream = Some(StreamInfo {
            index: 1,
            time_base: TimeBase::AUDIO_48K,
            duration: None,
            start_time: Some(0),
            params,
        });
        playback.video_decoder = Some(DecoderDebugInfo {
            implementation: "h264_vulkan".into(),
            hardware_accelerated: true,
        });
        playback.audio_decoder = Some(DecoderDebugInfo {
            implementation: "aac_sw".into(),
            hardware_accelerated: false,
        });

        let sections = playback.debug_info();
        let titles = sections
            .iter()
            .map(|section| section.title.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            titles,
            vec![
                "Playback",
                "Transport / pipeline",
                "Video",
                "Audio",
                "Timeline / seek"
            ]
        );
        let transport = sections
            .iter()
            .find(|section| section.title == "Transport / pipeline")
            .unwrap();
        assert!(transport.rows.iter().any(|(name, value)| {
            name == "compressed packet cap"
                && value == &format!("{PLAYBACK_PACKET_CHANNEL_CAP} / track")
        }));
        let video = sections
            .iter()
            .find(|section| section.title == "Video")
            .unwrap();
        assert!(video.rows.iter().any(|(name, value)| {
            name == "decoded queue" && value == &format!("{} / {}", 0, VIDEO_QUEUE_CAP)
        }));
        assert!(
            video
                .rows
                .iter()
                .any(|(name, value)| { name == "OxideAV decoder" && value == "h264_vulkan" })
        );
        assert!(
            video
                .rows
                .iter()
                .any(|(name, value)| { name == "decoder acceleration" && value == "hardware" })
        );
        let audio = sections
            .iter()
            .find(|section| section.title == "Audio")
            .unwrap();
        assert!(
            audio
                .rows
                .iter()
                .any(|(name, value)| name == "codec" && value == "aac")
        );
        assert!(
            audio
                .rows
                .iter()
                .any(|(name, value)| { name == "OxideAV decoder" && value == "aac_sw" })
        );
        assert!(
            audio
                .rows
                .iter()
                .any(|(name, value)| { name == "decoder acceleration" && value == "software" })
        );
    }

    #[test]
    fn debug_graph_does_not_invent_decoder_nodes_without_runtime_topology() {
        let (mut playback, _senders) = clock_test_playback();
        playback.decode_mode = DecodeMode::VulkanDirect;
        playback.video_decoder = Some(DecoderDebugInfo {
            implementation: "h264_vulkan_direct".into(),
            hardware_accelerated: true,
        });

        let graph = playback.debug_graph();

        assert!(graph.nodes.iter().any(|node| node.id == "media-source"));
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| !node.id.starts_with("oxideav-video-stage-"))
        );
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| node.title != "h264_vulkan_direct")
        );
    }

    #[test]
    fn playback_error_freezes_video_clock_and_position() {
        let (mut playback, _tx) = clock_test_playback();
        playback
            .video_clock
            .establish(90_000, Instant::now() - Duration::from_secs(1), true);

        playback.fail("synthetic playback failure".into());

        assert!(matches!(playback.state, PlaybackState::Error(_)));
        assert!(playback.video_clock.frozen_pts.is_some());
        let frozen_position = playback.position();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(playback.position(), frozen_position);
    }

    #[test]
    fn error_position_uses_stored_position_instead_of_live_clock() {
        let (mut playback, _tx) = clock_test_playback();
        playback.position = Duration::from_secs(42);
        playback.state = PlaybackState::Error("synthetic playback failure".into());
        playback
            .video_clock
            .establish(90_000, Instant::now() - Duration::from_secs(10), true);

        assert_eq!(playback.position(), Duration::from_secs(42));
    }

    #[test]
    fn video_clock_advances_without_decoded_frames() {
        let start = Instant::now();
        let mut clock = VideoClock::default();
        clock.establish(90_000, start, true);

        assert_eq!(
            clock.pts_at(start + Duration::from_millis(250), TimeBase::MPEG_TS),
            Some(112_500)
        );
    }

    #[test]
    fn video_clock_pause_resume_excludes_paused_wall_time() {
        let start = Instant::now();
        let mut clock = VideoClock::default();
        clock.establish(90_000, start, true);
        clock.pause(start + Duration::from_millis(250), TimeBase::MPEG_TS);
        assert_eq!(clock.frozen_pts, Some(112_500));

        let resume = start + Duration::from_secs(10);
        clock.play(resume);
        assert_eq!(
            clock.pts_at(resume + Duration::from_millis(250), TimeBase::MPEG_TS),
            Some(135_000)
        );
    }

    #[test]
    fn video_clock_computes_exact_future_frame_deadline() {
        let start = Instant::now();
        let mut clock = VideoClock::default();
        clock.establish(0, start, true);

        assert_eq!(
            clock.deadline_for(1_920, TimeBase::MPEG_TS),
            Some(start + Duration::from_nanos(21_333_334))
        );
    }

    #[test]
    fn playback_deadline_for_due_frame_uses_callers_instant_exactly() {
        let (mut playback, _tx) = clock_test_playback();
        let start = Instant::now();
        let now = start + Duration::from_secs(1);
        playback.video_clock.establish(0, start, true);
        playback.video_queue.clear();
        playback.video_queue.push_back(video_frame_lease(Some(0)));

        assert_eq!(playback.next_wake_deadline(now), Some(now));
    }

    #[test]
    fn video_scheduler_drops_older_due_frame_before_presenting_newer_due() {
        let (mut playback, _tx) = clock_test_playback();
        let start = Instant::now();
        playback.video_clock.establish(0, start, true);
        playback.video_queue.clear();
        playback.video_queue.push_back(video_frame_lease(Some(900)));
        playback
            .video_queue
            .push_back(video_frame_lease(Some(1_800)));

        let frame = playback
            .take_due_frame_at(start + Duration::from_millis(30))
            .expect("newer due frame");
        assert_eq!(frame.pts(), Some(1_800));
        assert_eq!(playback.diagnostics.dropped_video_frames, 1);
        assert!(playback.video_queue.is_empty());
    }

    #[test]
    fn video_scheduler_waits_for_future_frame_deadline() {
        let (mut playback, _tx) = clock_test_playback();
        let start = Instant::now();
        playback.video_clock.establish(0, start, true);
        playback.video_queue.clear();
        playback
            .video_queue
            .push_back(video_frame_lease(Some(1_920)));

        assert!(playback.take_due_frame_at(start).is_none());
        assert_eq!(
            playback.next_video_wake_deadline_at(start),
            Some(start + Duration::from_nanos(21_333_334))
        );
        assert!(
            playback
                .take_due_frame_at(start + Duration::from_nanos(21_333_334))
                .is_some()
        );
    }

    #[test]
    fn video_frame_without_pts_is_dropped_and_logged_path_remains_bounded() {
        let (mut playback, _tx) = clock_test_playback();
        playback
            .handle_session_message(SessionMsg::Frame {
                kind: MediaType::Video,
                frame: video_frame_lease(None),
            })
            .unwrap();

        assert!(playback.video_queue.is_empty());
        assert_eq!(playback.diagnostics.dropped_video_frames, 1);
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
    fn hls_quality_list_skips_unsupported_vp9_and_prefers_h264_at_same_height() {
        let mut h264 = hls_variant(
            "https://example.test/h264-720.m3u8",
            None,
            None,
            Some(1280),
            Some(720),
            None,
            2_000_000,
        );
        h264.codecs = Some("avc1.4D4020,mp4a.40.2".into());
        let mut vp9 = hls_variant(
            "https://example.test/vp9-720.m3u8",
            None,
            None,
            Some(1280),
            Some(720),
            None,
            3_000_000,
        );
        vp9.codecs = Some("vp09.00.40.08,mp4a.40.2".into());
        let set = quality_set_from_variants(vec![h264, vp9], 1).unwrap();
        assert_eq!(set.urls.len(), 1);
        assert_eq!(set.preferred_index, 0);
        assert_eq!(set.urls[0].as_str(), "https://example.test/h264-720.m3u8");
    }

    #[test]
    fn initial_quality_selection_uses_favourites_before_hls_default() {
        let qualities = vec![
            Quality::new("1080p60", "1080p60 (Source)"),
            Quality::new("720p60", "720p60"),
            Quality::new("480p", "480p"),
        ];

        assert_eq!(
            select_initial_quality_index(&qualities, 1, "480p;1080p60"),
            2
        );
        assert_eq!(
            select_initial_quality_index(&qualities, 1, "missing,1080p60 (Source)"),
            0
        );
        assert_eq!(select_initial_quality_index(&qualities, 1, "missing"), 1);
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

    #[test]
    fn pausing_during_seek_clears_resume_playing_intent() {
        let (mut playback, _tx) = clock_test_playback();
        playback.state = PlaybackState::Seeking;
        playback.seek_pending = Some(PendingSeek {
            generation: 1,
            requested: Duration::from_secs(10),
            prior_position: Duration::from_secs(5),
            resume_playing: true,
            barriers_remaining: 1,
            landing: None,
            rejected: false,
        });

        playback.pause();

        assert_eq!(playback.state, PlaybackState::Seeking);
        assert_eq!(
            playback
                .seek_pending
                .as_ref()
                .map(|pending| pending.resume_playing),
            Some(false)
        );
    }

    #[test]
    fn playing_during_seek_restores_resume_playing_intent() {
        let (mut playback, _tx) = clock_test_playback();
        playback.state = PlaybackState::Seeking;
        playback.seek_pending = Some(PendingSeek {
            generation: 1,
            requested: Duration::from_secs(10),
            prior_position: Duration::from_secs(5),
            resume_playing: false,
            barriers_remaining: 1,
            landing: None,
            rejected: false,
        });

        playback.play();

        assert_eq!(playback.state, PlaybackState::Seeking);
        assert_eq!(
            playback
                .seek_pending
                .as_ref()
                .map(|pending| pending.resume_playing),
            Some(true)
        );
    }

    #[test]
    fn session_sink_routes_control_audio_and_video_to_distinct_channels() {
        let (control_tx, control_rx) = mpsc::sync_channel(1);
        let (audio_tx, audio_rx) = mpsc::sync_channel(1);
        let (video_tx, video_rx) = mpsc::sync_channel(1);
        let depths = SessionChannelDepths::default();
        let mut sink = SessionSink::new(
            control_tx,
            audio_tx,
            video_tx,
            depths.clone(),
            PlaybackWake::noop(),
        );

        sink.send_control(SessionMsg::Finished).unwrap();
        assert_eq!(depths.control.load(Ordering::SeqCst), 1);
        assert!(matches!(control_rx.try_recv(), Ok(SessionMsg::Finished)));
        release_session_depth(&depths.control);
        assert_eq!(depths.control.load(Ordering::SeqCst), 0);
        assert!(matches!(audio_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(video_rx.try_recv(), Err(TryRecvError::Empty)));

        sink.send_legacy_track_message(
            MediaType::Audio,
            SessionMsg::Frame {
                kind: MediaType::Audio,
                frame: audio_frame_lease(16, 0),
            },
        )
        .unwrap();
        assert_eq!(depths.audio.load(Ordering::SeqCst), 1);
        assert!(matches!(
            audio_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Audio,
                ..
            })
        ));
        release_session_depth(&depths.audio);
        assert_eq!(depths.audio.load(Ordering::SeqCst), 0);
        assert!(matches!(control_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(video_rx.try_recv(), Err(TryRecvError::Empty)));

        sink.send_legacy_track_message(
            MediaType::Video,
            SessionMsg::Frame {
                kind: MediaType::Video,
                frame: video_frame_lease(Some(0)),
            },
        )
        .unwrap();
        assert_eq!(depths.video.load(Ordering::SeqCst), 1);
        assert!(matches!(
            video_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Video,
                ..
            })
        ));
        release_session_depth(&depths.video);
        assert_eq!(depths.video.load(Ordering::SeqCst), 0);
        assert!(matches!(control_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(audio_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn session_channel_depth_tracks_real_video_receive_boundary() {
        let (mut playback, senders) = clock_test_playback();
        let depth = Arc::clone(&playback.channel_depths.video);
        let mut sink = SessionTrackSink::new(
            MediaType::Video,
            senders._video_tx,
            Arc::clone(&depth),
            PlaybackWake::noop(),
            CancellationToken::new(),
        );

        sink.write_frame_lease(0, MediaType::Video, video_frame_lease(Some(180_000)))
            .unwrap();
        assert_eq!(depth.load(Ordering::SeqCst), 1);

        playback.pump_session();

        assert_eq!(depth.load(Ordering::SeqCst), 0);
        assert_eq!(playback.diagnostics.received_video_frames, 1);
    }

    #[test]
    fn session_track_sink_wakes_after_video_message_is_enqueued() {
        let (video_tx, video_rx) = mpsc::sync_channel(1);
        let (wake_tx, wake_rx) = mpsc::channel();
        let wake = PlaybackWake::new(move || {
            wake_tx.send(()).unwrap();
        });
        let wake_probe = wake.clone();
        let mut sink = SessionTrackSink::new(
            MediaType::Video,
            video_tx,
            Arc::new(AtomicUsize::new(0)),
            wake,
            CancellationToken::new(),
        );

        sink.write_frame_lease(0, MediaType::Video, video_frame_lease(Some(90_000)))
            .unwrap();

        assert!(matches!(
            video_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Video,
                ..
            })
        ));
        assert_eq!(wake_rx.try_recv(), Ok(()));
        assert!(wake_probe.take_pending().contains(PlaybackWakeKind::Video));
    }

    #[test]
    fn blocked_video_track_sink_does_not_block_audio_track_sink() {
        let (audio_tx, audio_rx) = mpsc::sync_channel(1);
        let (video_tx, video_rx) = mpsc::sync_channel(1);
        let cancellation = CancellationToken::new();
        let mut video_sink = SessionTrackSink::new(
            MediaType::Video,
            video_tx,
            Arc::new(AtomicUsize::new(0)),
            PlaybackWake::noop(),
            cancellation.clone(),
        );
        let mut audio_sink = SessionTrackSink::new(
            MediaType::Audio,
            audio_tx,
            Arc::new(AtomicUsize::new(0)),
            PlaybackWake::noop(),
            cancellation,
        );

        video_sink
            .write_frame_lease(0, MediaType::Video, video_frame_lease(Some(9_000)))
            .unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result =
                video_sink.write_frame_lease(0, MediaType::Video, video_frame_lease(Some(18_000)));
            let _ = done_tx.send(result);
        });

        std::thread::sleep(Duration::from_millis(10));
        assert!(matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));

        audio_sink
            .write_frame_lease(1, MediaType::Audio, audio_frame_lease(1_024, 0))
            .unwrap();
        assert!(matches!(
            audio_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Audio,
                ..
            })
        ));

        assert!(matches!(
            video_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SessionMsg::Frame {
                kind: MediaType::Video,
                ..
            }
        ));
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("video TrackSink should unblock after channel drains")
            .unwrap();
        assert!(matches!(
            video_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            SessionMsg::Frame {
                kind: MediaType::Video,
                ..
            }
        ));
        worker.join().unwrap();
    }

    #[test]
    fn full_video_lookahead_discards_provably_stale_frames_while_ingesting() {
        let (mut playback, _tx) = clock_test_playback();
        let now = Instant::now();
        playback.video_queue.clear();
        playback.video_clock = VideoClock {
            origin: None,
            frozen_pts: Some(2_700),
        };
        playback.diagnostics.dropped_video_frames = 0;

        assert!(
            playback
                .queue_video_frame_at(video_frame_lease(Some(900)), now)
                .is_none()
        );
        assert!(
            playback
                .queue_video_frame_at(video_frame_lease(Some(1_800)), now)
                .is_none()
        );
        assert!(
            playback
                .queue_video_frame_at(video_frame_lease(Some(2_700)), now)
                .is_none()
        );
        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![1_800, 2_700]
        );
        assert_eq!(playback.diagnostics.dropped_video_frames, 1);

        assert!(
            playback
                .queue_video_frame_at(video_frame_lease(Some(3_600)), now)
                .is_none()
        );
        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![2_700, 3_600]
        );
        assert_eq!(playback.diagnostics.dropped_video_frames, 2);

        let deferred = playback
            .queue_video_frame_at(video_frame_lease(Some(4_500)), now)
            .expect("future third frame must remain backpressured");
        assert_eq!(deferred.pts(), Some(4_500));
        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![2_700, 3_600]
        );
    }

    #[test]
    fn third_video_frame_stays_upstream_until_lookahead_has_space() {
        let (mut playback, _tx) = clock_test_playback();
        let (video_tx, video_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        playback.video_rx = video_rx;
        playback.video_queue.clear();
        playback
            .video_queue
            .push_back(video_frame_lease(Some(9_000)));
        playback
            .video_queue
            .push_back(video_frame_lease(Some(18_000)));
        playback.diagnostics.received_video_frames = 0;
        playback.diagnostics.dropped_video_frames = 0;

        video_tx
            .send(SessionMsg::Frame {
                kind: MediaType::Video,
                frame: video_frame_lease(Some(27_000)),
            })
            .unwrap();

        playback.pump_session();

        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![9_000, 18_000]
        );
        assert_eq!(playback.diagnostics.received_video_frames, 0);
        assert_eq!(playback.diagnostics.dropped_video_frames, 0);

        playback.video_queue.pop_front();
        playback.pump_session();

        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![18_000, 27_000]
        );
        assert_eq!(playback.diagnostics.received_video_frames, 1);
        assert_eq!(playback.diagnostics.dropped_video_frames, 0);
    }

    #[test]
    fn full_video_lookahead_does_not_starve_independent_audio_channel() {
        let (mut playback, senders) = clock_test_playback();
        let (video_tx, video_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        playback.video_rx = video_rx;

        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::F32);
        let driver =
            oxideav_sysaudio::driver_by_name("mock").expect("mock sysaudio driver available");
        let output =
            AudioOutput::open_with_driver(driver, &params, TimeBase::AUDIO_48K, false).unwrap();
        playback.audio_stream = Some(StreamInfo {
            index: 1,
            time_base: TimeBase::AUDIO_48K,
            duration: None,
            start_time: Some(0),
            params,
        });
        playback.audio_output = Some(output);
        playback.first_audio_seconds = None;
        playback.audio_anchor_seconds = None;

        playback.video_clock = VideoClock {
            origin: None,
            frozen_pts: Some(0),
        };
        playback.video_queue.clear();
        playback
            .video_queue
            .push_back(video_frame_lease(Some(9_000)));
        playback
            .video_queue
            .push_back(video_frame_lease(Some(18_000)));
        video_tx
            .send(SessionMsg::Frame {
                kind: MediaType::Video,
                frame: video_frame_lease(Some(27_000)),
            })
            .unwrap();

        senders
            .audio_tx
            .send(SessionMsg::Frame {
                kind: MediaType::Audio,
                frame: audio_frame_lease(1_024, 0),
            })
            .unwrap();

        assert_eq!(playback.diagnostics.received_audio_frames, 0);
        assert_eq!(
            playback
                .audio_output
                .as_ref()
                .expect("audio output")
                .queued_samples(),
            0
        );

        playback.pump_session();

        assert_eq!(
            playback.diagnostics.received_audio_frames, 1,
            "audio must continue while the video TrackSink is backpressured"
        );
        assert!(
            playback
                .audio_output
                .as_ref()
                .expect("audio output")
                .queued_samples()
                > 0,
            "pumped audio must reach the PCM timeline"
        );
        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![9_000, 18_000],
            "future video must remain upstream while the lookahead is full"
        );
        assert_eq!(playback.diagnostics.received_video_frames, 0);
        assert_eq!(playback.diagnostics.dropped_video_frames, 0);

        playback.video_queue.pop_front();
        playback.pump_session();

        assert_eq!(
            playback
                .video_queue
                .iter()
                .filter_map(FrameLease::pts)
                .collect::<Vec<_>>(),
            vec![18_000, 27_000],
            "the preserved future frame must arrive once lookahead has space"
        );
        assert_eq!(playback.diagnostics.received_video_frames, 1);
        assert_eq!(playback.diagnostics.dropped_video_frames, 0);
    }

    #[test]
    fn cancellation_unblocks_track_sink_waiting_on_full_channel() {
        let (video_tx, _video_rx) = mpsc::sync_channel(1);
        let cancellation = CancellationToken::new();
        let mut sink = SessionTrackSink::new(
            MediaType::Video,
            video_tx,
            Arc::new(AtomicUsize::new(0)),
            PlaybackWake::noop(),
            cancellation.clone(),
        );
        sink.write_frame_lease(0, MediaType::Video, video_frame_lease(Some(9_000)))
            .unwrap();

        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result =
                sink.write_frame_lease(0, MediaType::Video, video_frame_lease(Some(18_000)));
            let _ = done_tx.send(result);
        });

        std::thread::sleep(Duration::from_millis(10));
        cancellation.cancel();

        let result = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("cancellation should wake a blocked TrackSink");
        assert!(matches!(result, Err(ref error) if error.is_cancelled()));
        worker.join().unwrap();
    }

    #[test]
    fn automatic_selection_keeps_ranked_hardware_and_software_fallbacks_eligible() {
        let prefs = codec_preferences(DecodeMode::Auto);
        assert!(prefs.prefer.is_empty());
        assert!(prefs.exclude.is_empty());
        assert!(!prefs.no_hardware);
        assert!(!prefs.require_hardware);
    }

    #[test]
    fn vulkan_readback_selection_forces_readback_without_requiring_hardware_audio() {
        let prefs = codec_preferences(DecodeMode::VulkanReadback);
        assert_eq!(prefs.prefer, vec!["h264_vulkan"]);
        assert!(
            prefs
                .exclude
                .iter()
                .any(|name| name == "h264_vulkan_direct")
        );
        assert!(prefs.exclude.iter().any(|name| name == "h264_sw"));
        assert!(!prefs.require_hardware);
    }

    #[test]
    fn vulkan_direct_selection_is_strict_while_auto_keeps_fallbacks_eligible() {
        let direct = codec_preferences(DecodeMode::VulkanDirect);
        assert_eq!(direct.prefer, vec!["h264_vulkan_direct"]);
        assert!(direct.exclude.iter().any(|name| name == "h264_vulkan"));
        assert!(direct.exclude.iter().any(|name| name == "h264_sw"));
        assert!(!direct.require_hardware);

        let auto = codec_preferences(DecodeMode::Auto);
        assert!(auto.prefer.is_empty());
        assert!(auto.exclude.is_empty());
    }

    #[test]
    fn mediacodec_selection_is_strict_per_requested_presentation_contract() {
        let direct = codec_preferences(DecodeMode::MediaCodecDirect);
        assert_eq!(direct.prefer, vec!["h264_mediacodec_direct"]);
        assert!(
            direct
                .exclude
                .iter()
                .any(|name| name == "h264_mediacodec_readback")
        );
        assert!(direct.exclude.iter().any(|name| name == "h264_sw"));
        assert!(!direct.require_hardware);

        let readback = codec_preferences(DecodeMode::MediaCodecReadback);
        assert_eq!(readback.prefer, vec!["h264_mediacodec_readback"]);
        assert!(
            readback
                .exclude
                .iter()
                .any(|name| name == "h264_mediacodec_direct")
        );
        assert!(readback.exclude.iter().any(|name| name == "h264_sw"));
        assert!(!readback.require_hardware);
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
    fn twitch_transport_pts_are_rebased_to_first_real_av_timestamp() {
        let audio_start = 70.024_f64;
        let video_start = 70.094_f64;
        let origin = timeline_origin_seconds(Some(video_start), Some(audio_start), true).unwrap();
        assert!((origin - audio_start).abs() < f64::EPSILON);

        let video_stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: None,
            params: CodecParameters::video(CodecId::new("h264")),
        };
        assert_eq!(
            relative_stream_position(&video_stream, 6_308_460, origin),
            Some(Duration::from_millis(70))
        );
    }

    #[test]
    fn twitch_media_seek_converts_to_transport_pts() {
        let stream = StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(6_302_160),
            params: CodecParameters::video(CodecId::new("h264")),
        };
        assert_eq!(
            stream_pts_for_media_position(&stream, Duration::from_secs(300), 70.024),
            Some(33_302_160)
        );
    }

    #[test]
    fn post_seek_eof_survives_other_track_barrier_completion() {
        let (mut playback, _tx) = clock_test_playback();
        playback.audio_stream = Some(StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 48_000),
            duration: None,
            start_time: Some(0),
            params: CodecParameters::audio(CodecId::new("aac")),
        });
        playback.state = PlaybackState::Seeking;
        playback.position = Duration::from_secs(30);
        playback.video_eof = true;
        playback.audio_eof = true;
        playback.seek_pending = Some(PendingSeek {
            generation: 12,
            requested: Duration::from_secs(30),
            prior_position: Duration::from_secs(25),
            resume_playing: true,
            barriers_remaining: 2,
            landing: None,
            rejected: false,
        });

        playback
            .handle_session_message(SessionMsg::Barrier {
                kind: Some(MediaType::Audio),
                barrier: BarrierKind::SeekFlush {
                    generation: 12,
                    landed_pts: 2_700_000,
                    time_base: TimeBase::new(1, 90_000),
                },
            })
            .unwrap();
        assert!(
            !playback.audio_eof,
            "audio barrier must clear only the old audio EOF"
        );
        assert!(
            playback.video_eof,
            "video EOF must remain until the video barrier"
        );

        playback
            .handle_session_message(SessionMsg::EndOfStream(MediaType::Audio))
            .unwrap();
        assert!(playback.audio_eof);

        playback
            .handle_session_message(SessionMsg::Barrier {
                kind: Some(MediaType::Video),
                barrier: BarrierKind::SeekFlush {
                    generation: 12,
                    landed_pts: 2_700_000,
                    time_base: TimeBase::new(1, 90_000),
                },
            })
            .unwrap();
        assert!(
            playback.audio_eof,
            "completing the seek on video must not erase post-barrier audio EOF"
        );
        assert!(!playback.video_eof);
        assert!(playback.seek_pending.is_none());

        playback
            .handle_session_message(SessionMsg::EndOfStream(MediaType::Video))
            .unwrap();
        playback.update_end_state();
        assert!(matches!(playback.state, PlaybackState::Ended));
    }
    #[test]
    fn seek_waits_for_every_routed_barrier_before_reanchoring() {
        let (mut playback, _tx) = clock_test_playback();
        playback.state = PlaybackState::Seeking;
        playback.position = Duration::from_secs(30);
        playback.seek_pending = Some(PendingSeek {
            generation: 9,
            requested: Duration::from_secs(30),
            prior_position: Duration::from_secs(2),
            resume_playing: true,
            barriers_remaining: 2,
            landing: None,
            rejected: false,
        });

        playback
            .handle_seek_barrier(
                None,
                BarrierKind::SeekFlush {
                    generation: 9,
                    landed_pts: 2_700_000,
                    time_base: TimeBase::new(1, 90_000),
                },
            )
            .unwrap();
        assert!(playback.seek_pending.is_some());
        assert_eq!(playback.state, PlaybackState::Seeking);

        playback
            .handle_seek_barrier(
                None,
                BarrierKind::SeekFlush {
                    generation: 9,
                    landed_pts: 2_700_000,
                    time_base: TimeBase::new(1, 90_000),
                },
            )
            .unwrap();
        assert!(playback.seek_pending.is_none());
        assert_eq!(playback.position, Duration::from_secs(30));
        assert_eq!(playback.state, PlaybackState::Playing);
        assert!(!playback.first_frame_presented);
    }

    #[test]
    fn post_seek_epoch_drops_audio_before_video_landing() {
        let (mut playback, _tx) = clock_test_playback();
        playback.audio_stream = Some(StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(0),
            params: CodecParameters::audio(CodecId::new("aac")),
        });
        playback.post_seek_epoch = Some(PostSeekEpoch {
            floor: Duration::from_secs(30),
            audio_aligned: false,
            video_aligned: true,
            dropped_audio_frames: 0,
            dropped_video_frames: 0,
        });

        assert!(playback.drop_before_post_seek_epoch(MediaType::Audio, Some(29 * 90_000)));
        let epoch = playback.post_seek_epoch.expect("audio still unaligned");
        assert!(!epoch.audio_aligned);
        assert_eq!(epoch.dropped_audio_frames, 1);

        assert!(!playback.drop_before_post_seek_epoch(MediaType::Audio, Some(30 * 90_000)));
        assert!(
            playback.post_seek_epoch.is_none(),
            "epoch guard should clear after both tracks align"
        );
    }

    #[test]
    fn post_seek_epoch_applies_same_floor_to_video() {
        let (mut playback, _tx) = clock_test_playback();
        playback.post_seek_epoch = Some(PostSeekEpoch {
            floor: Duration::from_secs(30),
            audio_aligned: true,
            video_aligned: false,
            dropped_audio_frames: 0,
            dropped_video_frames: 0,
        });

        assert!(playback.drop_before_post_seek_epoch(MediaType::Video, Some(29 * 90_000)));
        assert_eq!(playback.diagnostics.dropped_video_frames, 1);
        let epoch = playback.post_seek_epoch.expect("video still unaligned");
        assert!(!epoch.video_aligned);
        assert_eq!(epoch.dropped_video_frames, 1);

        assert!(!playback.drop_before_post_seek_epoch(MediaType::Video, Some(30 * 90_000)));
        assert!(
            playback.post_seek_epoch.is_none(),
            "epoch guard should clear after both tracks align"
        );
    }

    #[test]
    fn seek_completion_waits_for_authoritative_audio_after_fresh_session() {
        let (mut playback, _tx) = clock_test_playback();
        playback.state = PlaybackState::Seeking;
        playback.position = Duration::from_secs(5);
        playback.audio_stream = Some(StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(6_302_160),
            params: CodecParameters::audio(CodecId::new("aac")),
        });
        playback.audio_output = None;
        playback.audio_anchor_seconds = Some(70.024);
        playback.seek_pending = Some(PendingSeek {
            generation: 11,
            requested: Duration::from_secs(5),
            prior_position: Duration::ZERO,
            resume_playing: false,
            barriers_remaining: 2,
            landing: None,
            rejected: false,
        });

        playback
            .handle_seek_barrier(
                None,
                BarrierKind::SeekFlush {
                    generation: 11,
                    landed_pts: 6_666_000,
                    time_base: TimeBase::new(1, 90_000),
                },
            )
            .unwrap();
        playback
            .handle_seek_barrier(
                None,
                BarrierKind::SeekFlush {
                    generation: 11,
                    landed_pts: 6_666_000,
                    time_base: TimeBase::new(1, 90_000),
                },
            )
            .unwrap();

        assert!(playback.seek_pending.is_none());
        assert_eq!(playback.state, PlaybackState::Paused);
        assert!(
            playback.audio_output.is_none(),
            "seek completion must not open sysaudio from provisional AAC metadata"
        );
        assert!(
            playback.audio_anchor_seconds.is_none(),
            "the first post-seek audio PTS must establish the new audio epoch"
        );

        let mut params = CodecParameters::audio(CodecId::new("aac"));
        params.sample_rate = Some(48_000);
        params.channels = Some(2);
        params.sample_format = Some(SampleFormat::S16);
        let authoritative = StreamInfo {
            index: 1,
            time_base: TimeBase::new(1, 90_000),
            duration: None,
            start_time: Some(6_302_160),
            params,
        };

        playback
            .handle_stream_update_with(authoritative, |stream, muted| {
                let driver = oxideav_sysaudio::driver_by_name("mock")
                    .expect("mock sysaudio driver available");
                AudioOutput::open_with_driver(driver, &stream.params, stream.time_base, muted)
            })
            .unwrap();

        let output = playback
            .audio_output
            .as_ref()
            .expect("authoritative post-seek update should open audio output");
        assert_eq!(output.queue_target_samples(), 24_000);
    }

    #[test]
    fn repeated_seek_input_coalesces_to_latest_target_while_one_is_in_flight() {
        let (mut playback, _tx) = clock_test_playback();
        playback.state = PlaybackState::Seeking;
        playback.position = Duration::from_secs(600);
        playback.seek_pending = Some(PendingSeek {
            generation: 7,
            requested: Duration::from_secs(600),
            prior_position: Duration::from_secs(5),
            resume_playing: true,
            barriers_remaining: 2,
            landing: None,
            rejected: false,
        });

        // No executor is installed in this unit fixture. These calls must not
        // try to dispatch generations 8/9: they only replace the one queued
        // destination and expose that latest target for subsequent relative
        // seek commands.
        playback.seek(Duration::from_secs(1_200));
        playback.seek(Duration::from_secs(1_800));

        assert_eq!(playback.position, Duration::from_secs(1_800));
        assert_eq!(playback.queued_seek, Some(Duration::from_secs(1_800)));
        assert_eq!(playback.seek_pending.as_ref().unwrap().generation, 7);
        assert_eq!(playback.state, PlaybackState::Seeking);
    }

    #[test]
    fn rejected_seek_restores_prior_position_and_disables_future_seeks() {
        let (mut playback, _tx) = clock_test_playback();
        playback.state = PlaybackState::Seeking;
        playback.position = Duration::from_secs(30);
        playback.seek_pending = Some(PendingSeek {
            generation: 4,
            requested: Duration::from_secs(30),
            prior_position: Duration::from_secs(3),
            resume_playing: false,
            barriers_remaining: 1,
            landing: None,
            rejected: false,
        });
        playback
            .handle_seek_barrier(None, BarrierKind::SeekRejected { generation: 4 })
            .unwrap();
        assert_eq!(playback.position, Duration::from_secs(3));
        assert_eq!(playback.state, PlaybackState::Paused);
        assert!(!playback.seek_supported);
    }

    #[test]
    fn av_pump_keeps_draining_when_video_is_ready_but_audio_is_low() {
        assert_eq!(
            av_pump_block_reason(AvPumpState {
                state: &PlaybackState::Playing,
                first_frame_presented: true,
                video_queue_len: VIDEO_QUEUE_CAP,
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
                video_queue_len: VIDEO_QUEUE_CAP,
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
