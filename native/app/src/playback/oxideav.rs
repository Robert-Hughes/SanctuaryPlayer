use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::time::Duration;

use ::oxideav::core::{Error, Frame, FrameLease, MediaType, Packet, StreamInfo, TimeBase};
use ::oxideav::pipeline::{Executor, ExecutorHandle, Job, JobSink};
use serde_json::json;
use url::Url;

use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

use super::PlaybackBackend;

const FRAME_CHANNEL_CAP: usize = 2;
const VIDEO_QUEUE_TARGET: usize = 4;
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

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
    video_queue: VecDeque<FrameLease>,
    timeline_origin_pts: Option<i64>,
    first_frame_presented: bool,
    sink_finished: bool,
}

enum SessionMsg {
    Started(Vec<StreamInfo>),
    Frame(FrameLease),
    Finished,
}

struct VideoSink {
    tx: SyncSender<SessionMsg>,
}

impl VideoSink {
    fn new(tx: SyncSender<SessionMsg>) -> Self {
        Self { tx }
    }

    fn send(&self, message: SessionMsg) -> ::oxideav::core::Result<()> {
        self.tx
            .send(message)
            .map_err(|_| Error::other("SanctuaryPlayer: playback receiver dropped"))
    }
}

impl JobSink for VideoSink {
    fn start(&mut self, streams: &[StreamInfo]) -> ::oxideav::core::Result<()> {
        self.send(SessionMsg::Started(streams.to_vec()))
    }

    fn write_packet(&mut self, _kind: MediaType, _packet: &Packet) -> ::oxideav::core::Result<()> {
        Err(Error::unsupported(
            "SanctuaryPlayer playback sink requires decoded video frames",
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
        if kind != MediaType::Video {
            return Ok(());
        }
        self.send(SessionMsg::Frame(frame))
    }

    fn finish(&mut self) -> ::oxideav::core::Result<()> {
        let _ = self.tx.send(SessionMsg::Finished);
        Ok(())
    }
}

impl OxidePlayback {
    pub fn open(source: VideoSource, m3u8_url: Url) -> Result<Self, String> {
        let input = format!("hls+{}", m3u8_url.as_str());
        let job_json = serde_json::to_string(&json!({
            "@in": { "all": [{ "from": input }] },
            "@display": { "video": [{ "from": "@in" }] },
        }))
        .map_err(|error| format!("build OxideAV playback job: {error}"))?;
        let job = Job::from_json(&job_json).map_err(|error| error.to_string())?;
        job.validate().map_err(|error| error.to_string())?;

        let mut registries = ::oxideav::Registries::new();
        oxideav_meta::register_all(&mut registries);

        let (tx, rx) = mpsc::sync_channel(FRAME_CHANNEL_CAP);
        let sink = Box::new(VideoSink::new(tx));
        let executor = Executor::new(&job, &registries)
            .with_sink_override("@display", sink)
            .with_threads(0)
            .spawn()
            .map_err(|error| format!("start OxideAV playback: {error}"))?;

        let streams = match rx.recv_timeout(OPEN_TIMEOUT) {
            Ok(SessionMsg::Started(streams)) => streams,
            Ok(_) => {
                executor.request_abort();
                let _ = executor.stop();
                return Err("OxideAV emitted video before stream initialisation".into());
            }
            Err(error) => {
                executor.request_abort();
                let _ = executor.stop();
                return Err(format!("waiting for OxideAV stream information: {error}"));
            }
        };
        let video_stream = streams
            .into_iter()
            .find(|stream| stream.params.media_type == MediaType::Video)
            .ok_or_else(|| "OxideAV source contains no video stream".to_owned())?;

        let duration = stream_duration(&video_stream);
        eprintln!(
            "SanctuaryPlayer: OxideAV software video stream codec={} {}x{} time_base={}/{}",
            video_stream.params.codec_id,
            video_stream.params.width.unwrap_or(0),
            video_stream.params.height.unwrap_or(0),
            video_stream.time_base.num(),
            video_stream.time_base.den(),
        );

        Ok(Self {
            source,
            state: PlaybackState::Paused,
            position: Duration::ZERO,
            duration,
            rate: 1.0,
            rates: vec![0.25, 0.5, 1.0, 1.5, 2.0],
            qualities: vec![Quality::new("hls-auto", "HLS (up to 720p)")],
            rx,
            executor: Some(executor),
            video_stream,
            video_queue: VecDeque::new(),
            timeline_origin_pts: None,
            first_frame_presented: false,
            sink_finished: false,
        })
    }

    fn pump_frames(&mut self) {
        let target = if matches!(self.state, PlaybackState::Paused) {
            usize::from(self.video_queue.is_empty())
        } else {
            VIDEO_QUEUE_TARGET
        };

        while self.video_queue.len() < target {
            match self.rx.try_recv() {
                Ok(SessionMsg::Started(_)) => {}
                Ok(SessionMsg::Frame(frame)) => {
                    if self.timeline_origin_pts.is_none() {
                        self.timeline_origin_pts = frame.pts();
                    }
                    self.video_queue.push_back(frame);
                }
                Ok(SessionMsg::Finished) => {
                    self.sink_finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.sink_finished = true;
                    break;
                }
            }
        }

        self.collect_executor_result();
        if self.sink_finished
            && self.video_queue.is_empty()
            && !matches!(self.state, PlaybackState::Error(_))
        {
            if self.first_frame_presented {
                self.state = PlaybackState::Ended;
            } else if self.executor.is_none() {
                self.state = PlaybackState::Error(
                    "OxideAV playback finished without producing a video frame".into(),
                );
            }
        }
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
            let message = format!("OxideAV playback failed: {error}");
            eprintln!("SanctuaryPlayer: {message}");
            self.state = PlaybackState::Error(message);
        }
    }

    fn frame_position(&self, frame: &FrameLease) -> Option<Duration> {
        let origin = self.timeline_origin_pts?;
        let pts = frame.pts()?;
        let seconds = self
            .video_stream
            .time_base
            .seconds_of(pts.saturating_sub(origin));
        (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
    }

    fn take_due_frame(&mut self) -> Option<FrameLease> {
        self.pump_frames();

        if !self.first_frame_presented {
            let frame = self.video_queue.pop_front()?;
            self.first_frame_presented = true;
            return Some(frame);
        }
        if !matches!(self.state, PlaybackState::Playing) {
            return None;
        }

        let mut due = None;
        while let Some(next) = self.video_queue.front() {
            let is_due = self
                .frame_position(next)
                .is_none_or(|frame_position| frame_position <= self.position);
            if !is_due {
                break;
            }
            due = self.video_queue.pop_front();
        }
        due
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
        if matches!(self.state, PlaybackState::Paused) {
            self.state = PlaybackState::Playing;
        }
    }

    fn pause(&mut self) {
        if matches!(self.state, PlaybackState::Playing) {
            self.state = PlaybackState::Paused;
        }
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
        self.pump_frames();
        if matches!(self.state, PlaybackState::Playing) {
            self.position = self.position.saturating_add(elapsed.mul_f32(self.rate));
            if self
                .duration
                .is_some_and(|duration| self.position >= duration)
            {
                self.position = self.duration.unwrap_or(self.position);
            }
        }
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
        if let Some(executor) = self.executor.take() {
            executor.request_abort();
            drop(executor);
        }
    }
}

fn stream_duration(stream: &StreamInfo) -> Option<Duration> {
    let ticks = stream.duration?;
    duration_from_ticks(stream.time_base, ticks)
}

fn duration_from_ticks(time_base: TimeBase, ticks: i64) -> Option<Duration> {
    let seconds = time_base.seconds_of(ticks);
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_stream_ticks_to_duration() {
        assert_eq!(
            duration_from_ticks(TimeBase::new(1, 90_000), 180_000),
            Some(Duration::from_secs(2))
        );
    }
}
