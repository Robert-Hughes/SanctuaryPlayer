mod dummy;
mod oxideav;

use std::time::Duration;

use ::oxideav::core::FrameLease;

use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

pub use self::oxideav::OxidePlayback;
pub use dummy::DummyPlayback;

/// Application-facing playback API. The concrete implementation owns media
/// scheduling while the renderer consumes retained decoded-frame leases.
pub trait PlaybackBackend: Send {
    fn open(&mut self, source: &VideoSource) -> Result<(), String>;
    fn source(&self) -> Option<&VideoSource>;
    fn state(&self) -> &PlaybackState;
    fn play(&mut self);
    fn pause(&mut self);
    fn position(&self) -> Duration;
    fn duration(&self) -> Option<Duration>;
    fn seek(&mut self, position: Duration);
    fn available_rates(&self) -> &[f32];
    fn playback_rate(&self) -> f32;
    fn set_playback_rate(&mut self, rate: f32);
    fn available_qualities(&self) -> &[Quality];
    fn quality(&self) -> Option<&Quality>;
    fn set_quality(&mut self, quality_id: &str);
    fn update(&mut self, elapsed: Duration);

    fn needs_animation(&self) -> bool {
        matches!(
            self.state(),
            PlaybackState::Playing | PlaybackState::Seeking
        )
    }

    fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        None
    }
}
