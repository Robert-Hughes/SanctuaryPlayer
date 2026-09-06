mod dummy;

use std::time::Duration;

use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

pub use dummy::DummyPlayback;

/// Application-facing playback API. The real OxideAV implementation will fit
/// behind this same boundary later.
pub trait PlaybackBackend {
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
}
