use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use ::oxideav::core::{
    CancellationToken, Error, Frame, FrameLease, MediaType, Packet, Rounding, StreamInfo, TimeBase,
    VideoColorInfo,
};
use ::oxideav::pipeline::{
    BarrierKind, ChannelCaps, CodecPreferences, Executor, ExecutorHandle, Job, JobSink, TrackSink,
    TrackSinkInfo,
};
use oxideav_hls::{HlsPlaylistInfo, HlsVariant};
use serde_json::json;
use url::Url;

use crate::audio_output::AudioOutput;
use crate::audio_timeline::QueueResult;
use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

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
const TRACK_SINK_BACKPRESSURE_WAIT: Duration = Duration::from_millis(20);

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
    pending_quality_switch: Option<PendingQualitySwitch>,
    queued_quality_index: Option<usize>,
    decode_mode: DecodeMode,
    muted: bool,
    wake: PlaybackWake,
    control_rx: Receiver<SessionMsg>,
    audio_rx: Receiver<SessionMsg>,
    video_rx: Receiver<SessionMsg>,
    executor: Option<ExecutorHandle>,
    video_stream: StreamInfo,
    audio_stream: Option<StreamInfo>,
    audio_output: Option<AudioOutput>,
    pending_audio_frame: Option<FrameLease>,
    video_queue: VecDeque<FrameLease>,
    video_clock: VideoClock,
    timeline_origin_seconds: Option<f64>,
    first_video_seconds: Option<f64>,
    first_audio_seconds: Option<f64>,
    audio_anchor_seconds: Option<f64>,
    first_frame_presented: bool,
    sink_finished: bool,
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
struct QualitySwitchIntent {
    target_index: usize,
    preserved_position: Duration,
    resume_playing: bool,
}

struct PendingQualitySwitch {
    intent: QualitySwitchIntent,
    receiver: Receiver<Result<PlaybackSession, String>>,
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
    Frame { kind: MediaType, frame: FrameLease },
    Barrier(BarrierKind),
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
            Self::Barrier(_) => "barrier",
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
    wake: PlaybackWake,
}

impl SessionSink {
    fn new(
        control_tx: SyncSender<SessionMsg>,
        audio_tx: SyncSender<SessionMsg>,
        video_tx: SyncSender<SessionMsg>,
        wake: PlaybackWake,
    ) -> Self {
        Self {
            control_tx,
            audio_tx,
            video_tx,
            wake,
        }
    }

    fn send_control(&mut self, message: SessionMsg) -> ::oxideav::core::Result<()> {
        let wake_kind = message.wake_kind();
        self.control_tx
            .send(message)
            .map_err(|_| Error::other("SanctuaryPlayer: control receiver dropped"))?;
        self.wake.wake(wake_kind);
        Ok(())
    }

    fn send_legacy_track_message(
        &mut self,
        kind: MediaType,
        message: SessionMsg,
    ) -> ::oxideav::core::Result<()> {
        let (tx, wake_kind, label) = match kind {
            MediaType::Audio => (&self.audio_tx, PlaybackWakeKind::Audio, "audio"),
            MediaType::Video => (&self.video_tx, PlaybackWakeKind::Video, "video"),
            _ => return Ok(()),
        };
        tx.send(message)
            .map_err(|_| Error::other(format!("SanctuaryPlayer: {label} receiver dropped")))?;
        self.wake.wake(wake_kind);
        Ok(())
    }
}

struct SessionTrackSink {
    kind: MediaType,
    tx: SyncSender<SessionMsg>,
    wake: PlaybackWake,
    cancellation: CancellationToken,
    blocked_sends: u64,
    last_backpressure_log: Option<Instant>,
}

impl SessionTrackSink {
    fn new(
        kind: MediaType,
        tx: SyncSender<SessionMsg>,
        wake: PlaybackWake,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            kind,
            tx,
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
            match self.tx.try_send(message) {
                Ok(()) => {
                    self.wake.wake(wake_kind);
                    return Ok(());
                }
                Err(TrySendError::Full(returned)) => {
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
        self.send(SessionMsg::Barrier(barrier))
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
        self.send_control(SessionMsg::Barrier(barrier))
    }

    fn finish(&mut self) -> ::oxideav::core::Result<()> {
        self.send_control(SessionMsg::Finished)
    }
}

struct PlaybackSession {
    control_rx: Receiver<SessionMsg>,
    audio_rx: Receiver<SessionMsg>,
    video_rx: Receiver<SessionMsg>,
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

struct RetiredSession {
    control_rx: Receiver<SessionMsg>,
    audio_rx: Receiver<SessionMsg>,
    video_rx: Receiver<SessionMsg>,
    executor: Option<ExecutorHandle>,
    audio_output: Option<AudioOutput>,
    pending_audio_frame: Option<FrameLease>,
    video_queue: VecDeque<FrameLease>,
}

fn retire_session(retired: RetiredSession) {
    let RetiredSession {
        control_rx,
        audio_rx,
        video_rx,
        executor,
        audio_output,
        pending_audio_frame,
        video_queue,
    } = retired;

    // Disconnect TrackSinks before joining the executor, then release all
    // application-owned media/audio resources on this worker as well.
    drop(control_rx);
    drop(audio_rx);
    drop(video_rx);
    drop(pending_audio_frame);
    drop(video_queue);
    drop(audio_output);
    if let Some(executor) = executor {
        stop_executor(executor);
    }
}

fn retired_opened_session(session: PlaybackSession) -> RetiredSession {
    RetiredSession {
        control_rx: session.control_rx,
        audio_rx: session.audio_rx,
        video_rx: session.video_rx,
        executor: session.executor,
        audio_output: session.audio_output,
        pending_audio_frame: None,
        video_queue: VecDeque::new(),
    }
}

fn spawn_quality_open_worker<F>(
    retired: Option<RetiredSession>,
    variant_url: Url,
    decode_mode: DecodeMode,
    wake: PlaybackWake,
    opener: F,
) -> Receiver<Result<PlaybackSession, String>>
where
    F: FnOnce(&Url, DecodeMode, PlaybackWake) -> Result<PlaybackSession, String> + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("sanctuary-quality-switch".into())
        .spawn(move || {
            if let Some(retired) = retired {
                retire_session(retired);
            }
            let result = opener(&variant_url, decode_mode, wake.clone());
            match sender.send(result) {
                Ok(()) => {}
                Err(error) => {
                    if let Ok(session) = error.0 {
                        retire_session(retired_opened_session(session));
                    }
                }
            }
            wake.wake(PlaybackWakeKind::Control);
        })
        .expect("spawn Sanctuary quality-switch worker");
    receiver
}

fn open_variant_session(
    variant_url: &Url,
    decode_mode: DecodeMode,
    wake: PlaybackWake,
) -> Result<PlaybackSession, String> {
    decode_mode.validate_current_platform()?;
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
    let sink = Box::new(SessionSink::new(control_tx, audio_tx, video_tx, wake));
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
        .with_threads(0)
        .spawn()
        .map_err(|error| format!("start OxideAV playback: {error}"))?;

    let streams = match control_rx.recv_timeout(OPEN_TIMEOUT) {
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
        initial_qualities: &str,
        decode_mode: DecodeMode,
        muted: bool,
        wake: PlaybackWake,
    ) -> Result<Self, String> {
        let quality_set = inspect_hls_qualities(&m3u8_url)?;
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
        let session = open_variant_session(&selected_url, decode_mode, wake.clone())?;

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
            active_quality_index: initial_quality_index,
            pending_quality_switch: None,
            queued_quality_index: None,
            decode_mode,
            muted,
            wake,
            control_rx: session.control_rx,
            audio_rx: session.audio_rx,
            video_rx: session.video_rx,
            executor: session.executor,
            video_stream: session.video_stream,
            audio_stream: session.audio_stream,
            audio_output: session.audio_output,
            pending_audio_frame: None,
            video_queue: VecDeque::new(),
            video_clock: VideoClock::default(),
            timeline_origin_seconds: session.timeline_origin_seconds,
            first_video_seconds: session.first_video_seconds,
            first_audio_seconds: session.first_audio_seconds,
            audio_anchor_seconds: session.first_audio_seconds,
            first_frame_presented: false,
            sink_finished: false,
            seek_pending: None,
            post_seek_epoch: None,
            queued_seek: None,
            seek_supported: true,
            diagnostics: PlaybackDiagnostics::new(),
        })
    }

    fn detach_session(&mut self) -> RetiredSession {
        let (_control_placeholder_tx, control_placeholder_rx) = mpsc::sync_channel(1);
        let old_control_rx = std::mem::replace(&mut self.control_rx, control_placeholder_rx);
        let (_audio_placeholder_tx, audio_placeholder_rx) = mpsc::sync_channel(1);
        let old_audio_rx = std::mem::replace(&mut self.audio_rx, audio_placeholder_rx);
        let (_video_placeholder_tx, video_placeholder_rx) = mpsc::sync_channel(1);
        let old_video_rx = std::mem::replace(&mut self.video_rx, video_placeholder_rx);

        RetiredSession {
            control_rx: old_control_rx,
            audio_rx: old_audio_rx,
            video_rx: old_video_rx,
            executor: self.executor.take(),
            audio_output: self.audio_output.take(),
            pending_audio_frame: self.pending_audio_frame.take(),
            video_queue: std::mem::take(&mut self.video_queue),
        }
    }

    fn install_session(&mut self, session: PlaybackSession) {
        self.control_rx = session.control_rx;
        self.audio_rx = session.audio_rx;
        self.video_rx = session.video_rx;
        self.executor = session.executor;
        self.video_stream = session.video_stream;
        self.audio_stream = session.audio_stream;
        self.audio_output = session.audio_output;
        self.pending_audio_frame = None;
        self.duration = session.duration;
        self.rates = session.rates;
        self.position = Duration::ZERO;
        self.timeline_origin_seconds = session.timeline_origin_seconds;
        self.first_video_seconds = session.first_video_seconds;
        self.first_audio_seconds = session.first_audio_seconds;
        self.audio_anchor_seconds = session.first_audio_seconds;
        self.first_frame_presented = false;
        self.sink_finished = false;
        self.seek_pending = None;
        self.post_seek_epoch = None;
        self.queued_seek = None;
        self.seek_supported = true;
        self.video_queue.clear();
        self.video_clock.reset();
        self.diagnostics = PlaybackDiagnostics::new();
    }

    fn start_quality_worker(
        &mut self,
        intent: QualitySwitchIntent,
        retired: Option<RetiredSession>,
    ) {
        let new_quality = self.qualities[intent.target_index].label.clone();
        let new_url = self.quality_urls[intent.target_index].clone();
        log::info!(
            "SanctuaryPlayer: quality switch worker start new={} variant={}",
            new_quality,
            new_url
        );
        let receiver = spawn_quality_open_worker(
            retired,
            new_url,
            self.decode_mode,
            self.wake.clone(),
            open_variant_session,
        );
        self.pending_quality_switch = Some(PendingQualitySwitch { intent, receiver });
    }

    fn begin_quality_switch(&mut self, index: usize) -> Result<(), String> {
        if index >= self.qualities.len() || index >= self.quality_urls.len() {
            return Err(format!("quality index {index} is out of range"));
        }

        self.quality_index = index;
        if let Some(pending) = self.pending_quality_switch.as_ref() {
            self.queued_quality_index = (index != pending.intent.target_index).then_some(index);
            log::info!(
                "SanctuaryPlayer: quality switch coalesced in_flight={} latest={}",
                self.qualities[pending.intent.target_index].label,
                self.qualities[index].label,
            );
            return Ok(());
        }
        if index == self.active_quality_index {
            return Ok(());
        }

        let now = Instant::now();
        let resume_playing = matches!(self.state, PlaybackState::Playing)
            || matches!(self.state, PlaybackState::Seeking)
                && self
                    .seek_pending
                    .as_ref()
                    .is_some_and(|pending| pending.resume_playing);
        if matches!(self.state, PlaybackState::Playing) {
            self.video_clock.pause(now, self.video_stream.time_base);
            self.update_position_at(now);
        }
        let preserved_position = self.position;
        if let Some(audio) = self.audio_output.as_mut() {
            let _ = audio.set_paused(true);
        }

        let old_quality = self.qualities[self.active_quality_index].label.clone();
        let new_quality = self.qualities[index].label.clone();
        let new_url = self.quality_urls[index].clone();
        log::info!(
            "SanctuaryPlayer: quality switch begin old={} new={} variant={} preserve={:.3}s resume_playing={}",
            old_quality,
            new_quality,
            new_url,
            preserved_position.as_secs_f64(),
            resume_playing
        );

        let retired = self.detach_session();
        self.state = PlaybackState::Loading;
        self.position = preserved_position;
        self.video_clock.reset();
        self.seek_pending = None;
        self.post_seek_epoch = None;
        self.queued_seek = None;
        self.queued_quality_index = None;
        self.start_quality_worker(
            QualitySwitchIntent {
                target_index: index,
                preserved_position,
                resume_playing,
            },
            Some(retired),
        );
        Ok(())
    }

    fn finish_quality_switch_with_seek<S>(
        &mut self,
        intent: QualitySwitchIntent,
        session: PlaybackSession,
        seek_after_open: S,
    ) -> Result<(), String>
    where
        S: FnOnce(&mut Self, Duration, bool) -> Result<(), String>,
    {
        let new_quality = self.qualities[intent.target_index].label.clone();
        let new_url = self.quality_urls[intent.target_index].clone();
        self.state = PlaybackState::Paused;
        self.install_session(session);
        self.quality_index = intent.target_index;
        self.active_quality_index = intent.target_index;

        let target = self.duration.map_or(intent.preserved_position, |duration| {
            intent.preserved_position.min(duration)
        });
        if target.is_zero() {
            if intent.resume_playing {
                self.play();
            }
            log::info!(
                "SanctuaryPlayer: quality switch complete active={} variant={} position=0s",
                new_quality,
                new_url
            );
            return Ok(());
        }

        // The replacement session remains paused until its preserved-position
        // seek lands, so no 0:00 frame or PCM can leak into presentation.
        seek_after_open(self, target, intent.resume_playing).map_err(|error| {
            format!("seek replacement HLS quality to preserved position: {error}")
        })?;
        log::info!(
            "SanctuaryPlayer: quality switch active={} variant={} seeking={:.3}s resume_playing={}",
            new_quality,
            new_url,
            target.as_secs_f64(),
            intent.resume_playing
        );
        Ok(())
    }

    fn poll_quality_switch(&mut self) {
        let result = {
            let Some(pending) = self.pending_quality_switch.as_ref() else {
                return;
            };
            match pending.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("quality-switch worker stopped unexpectedly".into()))
                }
            }
        };
        let Some(result) = result else {
            return;
        };
        let pending = self
            .pending_quality_switch
            .take()
            .expect("quality-switch result implies pending worker");
        let intent = pending.intent;

        if let Some(next_index) = self.queued_quality_index.take() {
            let retired = match result {
                Ok(session) => Some(retired_opened_session(session)),
                Err(error) => {
                    log::info!(
                        "SanctuaryPlayer: superseded quality switch failed before latest request: {error}"
                    );
                    None
                }
            };
            let next_intent = QualitySwitchIntent {
                target_index: next_index,
                preserved_position: intent.preserved_position,
                resume_playing: intent.resume_playing,
            };
            self.start_quality_worker(next_intent, retired);
            return;
        }

        match result {
            Ok(session) => {
                if let Err(error) = self.finish_quality_switch_with_seek(
                    intent,
                    session,
                    |playback, target, resume| {
                        playback.dispatch_seek(target, Duration::ZERO, resume)
                    },
                ) {
                    self.fail(error);
                }
            }
            Err(error) => {
                self.quality_index = self.active_quality_index;
                self.fail(format!(
                    "switch HLS quality to {}: {error}",
                    self.qualities[intent.target_index].label
                ));
            }
        }
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
            SessionMsg::Barrier(barrier) => self.handle_seek_barrier(barrier),
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

    fn handle_seek_barrier(&mut self, barrier: BarrierKind) -> Result<(), String> {
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
                self.audio_output = Some(
                    AudioOutput::open(&stream.params, stream.time_base, self.muted)
                        .map_err(|error| format!("reopen audio output after seek: {error}"))?,
                );
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

    fn fail(&mut self, message: String) {
        log::error!("SanctuaryPlayer: {message}");
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
        if !self.sink_finished || !self.video_queue.is_empty() || self.pending_audio_frame.is_some()
        {
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
        Some(frame)
    }

    fn take_due_frame(&mut self) -> Option<FrameLease> {
        self.take_due_frame_at(Instant::now())
    }

    fn next_video_wake_deadline_at(&self, now: Instant) -> Option<Instant> {
        if !matches!(self.state, PlaybackState::Playing) {
            return None;
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
        if !matches!(self.state, PlaybackState::Playing) {
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
        let resume_playing = matches!(self.state, PlaybackState::Playing);

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
        if let Err(error) = self.begin_quality_switch(index) {
            self.fail(error);
        }
    }

    fn update(&mut self, _elapsed: Duration) {
        self.poll_quality_switch();
        if self.pending_quality_switch.is_some() {
            return;
        }
        self.pump_session();
        self.update_position_at(Instant::now());
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
            // readback, then h264_sw; FreeBSD ranks VDPAU before h264_sw. Factory
            // failures therefore walk the same quality order without making the
            // user's automatic request strict.
            CodecPreferences::default()
        }
        DecodeMode::Cpu => CodecPreferences {
            no_hardware: true,
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
                active_quality_index: 0,
                pending_quality_switch: None,
                queued_quality_index: None,
                decode_mode: DecodeMode::Cpu,
                muted: false,
                wake: PlaybackWake::noop(),
                control_rx,
                audio_rx,
                video_rx,
                executor: None,
                video_stream,
                audio_stream: None,
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
                sink_finished: false,
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

    fn replacement_test_session() -> PlaybackSession {
        let (_control_tx, control_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        let (_audio_tx, audio_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        let (_video_tx, video_rx) = mpsc::sync_channel(SESSION_CHANNEL_CAP);
        PlaybackSession {
            control_rx,
            audio_rx,
            video_rx,
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

    fn fake_quality_seek(
        playback: &mut OxidePlayback,
        target: Duration,
        resume_playing: bool,
    ) -> Result<(), String> {
        playback.video_queue.clear();
        playback.position = target;
        playback.state = PlaybackState::Seeking;
        playback.seek_pending = Some(PendingSeek {
            generation: 77,
            requested: target,
            prior_position: Duration::ZERO,
            resume_playing,
            barriers_remaining: 1,
            landing: None,
            rejected: false,
        });
        Ok(())
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
            .finish_quality_switch_with_seek(
                QualitySwitchIntent {
                    target_index: 0,
                    preserved_position: Duration::from_secs(12),
                    resume_playing: true,
                },
                replacement_test_session(),
                fake_quality_seek,
            )
            .unwrap();

        assert_eq!(playback.quality().unwrap().id, "1080p60");
        assert_eq!(playback.quality_index, 0);
        assert_eq!(playback.active_quality_index, 0);
        assert_eq!(playback.position, Duration::from_secs(12));
        assert_eq!(playback.state, PlaybackState::Seeking);
        assert!(playback.video_queue.is_empty());
        assert!(!playback.first_frame_presented);
        assert_eq!(playback.duration, Some(Duration::from_secs(30)));
        assert_eq!(playback.rates, vec![0.5, 1.0, 2.0]);

        playback
            .handle_seek_barrier(BarrierKind::SeekFlush {
                generation: 77,
                landed_pts: 12 * 90_000,
                time_base: TimeBase::new(1, 90_000),
            })
            .unwrap();
        assert_eq!(playback.position, Duration::from_secs(12));
        assert_eq!(playback.state, PlaybackState::Playing);
    }

    #[test]
    fn quality_open_worker_runs_reconstruction_off_the_caller_thread() {
        let (release_tx, release_rx) = mpsc::channel();
        let (wake_tx, wake_rx) = mpsc::channel();
        let wake = PlaybackWake::new(move || {
            wake_tx.send(()).unwrap();
        });
        let wake_probe = wake.clone();

        let result_rx = spawn_quality_open_worker(
            None,
            Url::parse("https://example.test/1080.m3u8").unwrap(),
            DecodeMode::VdpauDirect,
            wake,
            move |url, decode_mode, _wake| {
                assert_eq!(url.as_str(), "https://example.test/1080.m3u8");
                assert_eq!(decode_mode, DecodeMode::VdpauDirect);
                release_rx.recv().unwrap();
                Ok(replacement_test_session())
            },
        );

        assert!(matches!(result_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(wake_rx.try_recv(), Err(TryRecvError::Empty)));

        release_tx.send(()).unwrap();
        assert!(
            result_rx
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker completion should wake the event loop");
        assert!(
            wake_probe
                .take_pending()
                .contains(PlaybackWakeKind::Control)
        );
    }

    #[test]
    fn repeated_quality_requests_keep_only_the_latest_target() {
        let (mut playback, _old_tx) = clock_test_playback();
        playback.qualities = vec![
            Quality::new("1080p60", "1080p60 (Source)"),
            Quality::new("720p60", "720p60"),
            Quality::new("160p", "160p"),
        ];
        playback.quality_urls = vec![
            Url::parse("https://example.test/1080.m3u8").unwrap(),
            Url::parse("https://example.test/720.m3u8").unwrap(),
            Url::parse("https://example.test/160.m3u8").unwrap(),
        ];
        playback.quality_index = 0;
        playback.active_quality_index = 1;
        let (_result_tx, result_rx) = mpsc::channel();
        playback.pending_quality_switch = Some(PendingQualitySwitch {
            intent: QualitySwitchIntent {
                target_index: 0,
                preserved_position: Duration::from_secs(12),
                resume_playing: true,
            },
            receiver: result_rx,
        });

        playback.set_quality("160p");
        assert_eq!(playback.quality_index, 2);
        assert_eq!(playback.queued_quality_index, Some(2));

        playback.set_quality("1080p60");
        assert_eq!(playback.quality_index, 0);
        assert_eq!(playback.queued_quality_index, None);

        playback.set_quality("160p");
        assert_eq!(playback.quality_index, 2);
        assert_eq!(playback.queued_quality_index, Some(2));
    }

    #[test]
    fn session_sink_routes_control_audio_and_video_to_distinct_channels() {
        let (control_tx, control_rx) = mpsc::sync_channel(1);
        let (audio_tx, audio_rx) = mpsc::sync_channel(1);
        let (video_tx, video_rx) = mpsc::sync_channel(1);
        let mut sink = SessionSink::new(control_tx, audio_tx, video_tx, PlaybackWake::noop());

        sink.send_control(SessionMsg::Finished).unwrap();
        assert!(matches!(control_rx.try_recv(), Ok(SessionMsg::Finished)));
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
        assert!(matches!(
            audio_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Audio,
                ..
            })
        ));
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
        assert!(matches!(
            video_rx.try_recv(),
            Ok(SessionMsg::Frame {
                kind: MediaType::Video,
                ..
            })
        ));
        assert!(matches!(control_rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(matches!(audio_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn session_track_sink_wakes_after_video_message_is_enqueued() {
        let (video_tx, video_rx) = mpsc::sync_channel(1);
        let (wake_tx, wake_rx) = mpsc::channel();
        let wake = PlaybackWake::new(move || {
            wake_tx.send(()).unwrap();
        });
        let wake_probe = wake.clone();
        let mut sink =
            SessionTrackSink::new(MediaType::Video, video_tx, wake, CancellationToken::new());

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
            PlaybackWake::noop(),
            cancellation.clone(),
        );
        let mut audio_sink = SessionTrackSink::new(
            MediaType::Audio,
            audio_tx,
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
    fn quality_switch_keeps_a_paused_session_paused() {
        let (mut playback, _old_tx) = clock_test_playback();
        playback.state = PlaybackState::Paused;
        playback.position = Duration::from_secs(7);
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
            .finish_quality_switch_with_seek(
                QualitySwitchIntent {
                    target_index: 0,
                    preserved_position: Duration::from_secs(7),
                    resume_playing: false,
                },
                replacement_test_session(),
                fake_quality_seek,
            )
            .unwrap();

        assert_eq!(playback.state, PlaybackState::Seeking);
        assert_eq!(playback.position, Duration::from_secs(7));
        assert_eq!(playback.active_quality_index, 0);

        playback
            .handle_seek_barrier(BarrierKind::SeekFlush {
                generation: 77,
                landed_pts: 7 * 90_000,
                time_base: TimeBase::new(1, 90_000),
            })
            .unwrap();
        assert_eq!(playback.state, PlaybackState::Paused);
        assert_eq!(playback.position, Duration::from_secs(7));
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
            .handle_seek_barrier(BarrierKind::SeekFlush {
                generation: 9,
                landed_pts: 2_700_000,
                time_base: TimeBase::new(1, 90_000),
            })
            .unwrap();
        assert!(playback.seek_pending.is_some());
        assert_eq!(playback.state, PlaybackState::Seeking);

        playback
            .handle_seek_barrier(BarrierKind::SeekFlush {
                generation: 9,
                landed_pts: 2_700_000,
                time_base: TimeBase::new(1, 90_000),
            })
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
            .handle_seek_barrier(BarrierKind::SeekFlush {
                generation: 11,
                landed_pts: 6_666_000,
                time_base: TimeBase::new(1, 90_000),
            })
            .unwrap();
        playback
            .handle_seek_barrier(BarrierKind::SeekFlush {
                generation: 11,
                landed_pts: 6_666_000,
                time_base: TimeBase::new(1, 90_000),
            })
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
            .handle_seek_barrier(BarrierKind::SeekRejected { generation: 4 })
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
