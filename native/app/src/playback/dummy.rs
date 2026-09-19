use std::time::Duration;

use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

use super::PlaybackBackend;

const DUMMY_DURATION: Duration = Duration::from_secs(3 * 3600 + 42 * 60 + 17);
const SEEK_DELAY: Duration = Duration::from_millis(180);

pub struct DummyPlayback {
    source: Option<VideoSource>,
    state: PlaybackState,
    position: Duration,
    duration: Duration,
    rate: f32,
    rates: Vec<f32>,
    qualities: Vec<Quality>,
    quality_index: usize,
    pending_seek: Option<PendingSeek>,
}

struct PendingSeek {
    target: Duration,
    remaining: Duration,
    resume_playing: bool,
}

impl Default for DummyPlayback {
    fn default() -> Self {
        Self {
            source: None,
            state: PlaybackState::Paused,
            position: Duration::ZERO,
            duration: DUMMY_DURATION,
            rate: 1.0,
            rates: vec![0.25, 0.5, 1.0, 1.5, 2.0],
            qualities: vec![
                Quality::new("source", "Source"),
                Quality::new("1080p60", "1080p60"),
                Quality::new("720p60", "720p60"),
                Quality::new("480p", "480p"),
            ],
            quality_index: 2,
            pending_seek: None,
        }
    }
}

impl DummyPlayback {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PlaybackBackend for DummyPlayback {
    fn open(&mut self, source: &VideoSource) -> Result<(), String> {
        self.source = Some(source.clone());
        self.position = source
            .start_time
            .unwrap_or(Duration::ZERO)
            .min(self.duration);
        self.state = PlaybackState::Paused;
        self.pending_seek = None;
        self.rate = 1.0;
        self.quality_index = 2;
        Ok(())
    }

    fn source(&self) -> Option<&VideoSource> {
        self.source.as_ref()
    }

    fn state(&self) -> &PlaybackState {
        &self.state
    }

    fn intends_playing(&self) -> bool {
        matches!(
            self.state,
            PlaybackState::Playing | PlaybackState::Buffering
        ) || self
            .pending_seek
            .as_ref()
            .is_some_and(|seek| seek.resume_playing)
    }

    fn play(&mut self) {
        if self.source.is_none() || matches!(self.state, PlaybackState::Ended) {
            return;
        }
        if let Some(seek) = self.pending_seek.as_mut() {
            seek.resume_playing = true;
        } else {
            self.state = PlaybackState::Playing;
        }
    }

    fn pause(&mut self) {
        if self.source.is_none() {
            return;
        }
        if let Some(seek) = self.pending_seek.as_mut() {
            seek.resume_playing = false;
        } else if !matches!(self.state, PlaybackState::Ended) {
            self.state = PlaybackState::Paused;
        }
    }

    fn position(&self) -> Duration {
        self.pending_seek
            .as_ref()
            .map(|seek| seek.target)
            .unwrap_or(self.position)
    }

    fn duration(&self) -> Option<Duration> {
        self.source.as_ref().map(|_| self.duration)
    }

    fn seek(&mut self, position: Duration) {
        if self.source.is_none() {
            return;
        }
        let resume_playing = matches!(self.state, PlaybackState::Playing)
            || self
                .pending_seek
                .as_ref()
                .is_some_and(|seek| seek.resume_playing);
        self.pending_seek = Some(PendingSeek {
            target: position.min(self.duration),
            remaining: SEEK_DELAY,
            resume_playing,
        });
        self.state = PlaybackState::Seeking;
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
        self.source
            .as_ref()
            .map(|_| &self.qualities[self.quality_index])
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

    fn update(&mut self, elapsed: Duration) {
        if let Some(mut seek) = self.pending_seek.take() {
            if elapsed >= seek.remaining {
                self.position = seek.target;
                if self.position >= self.duration {
                    self.state = PlaybackState::Ended;
                } else {
                    self.state = if seek.resume_playing {
                        PlaybackState::Playing
                    } else {
                        PlaybackState::Paused
                    };
                }
            } else {
                seek.remaining -= elapsed;
                self.pending_seek = Some(seek);
            }
            return;
        }

        if !matches!(self.state, PlaybackState::Playing) {
            return;
        }
        let advanced = elapsed.mul_f32(self.rate);
        self.position = self.position.saturating_add(advanced);
        if self.position >= self.duration {
            self.position = self.duration;
            self.state = PlaybackState::Ended;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoSource;

    fn source() -> VideoSource {
        VideoSource::parse("https://twitch.tv/videos/2386400830?t=1h").unwrap()
    }

    #[test]
    fn opens_at_requested_start_time_and_starts_paused() {
        let mut player = DummyPlayback::new();
        player.open(&source()).unwrap();
        assert_eq!(player.position(), Duration::from_secs(3600));
        assert_eq!(player.state(), &PlaybackState::Paused);
    }

    #[test]
    fn play_advances_using_playback_rate() {
        let mut player = DummyPlayback::new();
        player.open(&source()).unwrap();
        player.set_playback_rate(2.0);
        player.play();
        player.update(Duration::from_secs(3));
        assert_eq!(player.position(), Duration::from_secs(3606));
    }

    #[test]
    fn seeking_exposes_target_immediately_then_resumes_prior_state() {
        let mut player = DummyPlayback::new();
        player.open(&source()).unwrap();
        player.play();
        player.seek(Duration::from_secs(5000));
        assert_eq!(player.state(), &PlaybackState::Seeking);
        assert_eq!(player.position(), Duration::from_secs(5000));
        player.update(SEEK_DELAY);
        assert_eq!(player.state(), &PlaybackState::Playing);
        assert_eq!(player.position(), Duration::from_secs(5000));
    }

    #[test]
    fn paused_seek_stays_paused_and_clamps_to_duration() {
        let mut player = DummyPlayback::new();
        player.open(&source()).unwrap();
        player.seek(Duration::from_secs(99_999));
        player.update(SEEK_DELAY);
        assert_eq!(player.position(), DUMMY_DURATION);
        assert_eq!(player.state(), &PlaybackState::Ended);
    }

    #[test]
    fn quality_selection_changes_only_to_known_values() {
        let mut player = DummyPlayback::new();
        player.open(&source()).unwrap();
        assert_eq!(player.quality().unwrap().id, "720p60");
        player.set_quality("1080p60");
        assert_eq!(player.quality().unwrap().id, "1080p60");
        player.set_quality("imaginary");
        assert_eq!(player.quality().unwrap().id, "1080p60");
    }
}
