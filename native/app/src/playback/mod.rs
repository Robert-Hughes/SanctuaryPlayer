mod dummy;
mod oxideav;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use ::oxideav::core::FrameLease;

use crate::model::{PlaybackState, Quality};
use crate::video::VideoSource;

pub use self::oxideav::OxidePlayback;
pub use dummy::DummyPlayback;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecodeMode {
    #[default]
    Cpu,
    VdpauReadback,
    VdpauDirect,
}

impl DecodeMode {
    pub const fn platform_default() -> Self {
        if cfg!(target_os = "freebsd") {
            Self::VdpauDirect
        } else {
            Self::Cpu
        }
    }

    const fn is_supported_with_vdpau(self, vdpau_supported: bool) -> bool {
        match self {
            Self::Cpu => true,
            Self::VdpauReadback | Self::VdpauDirect => vdpau_supported,
        }
    }

    pub const fn is_supported_on_current_platform(self) -> bool {
        self.is_supported_with_vdpau(cfg!(target_os = "freebsd"))
    }

    fn validate_for_platform(self, vdpau_supported: bool, platform: &str) -> Result<(), String> {
        if self.is_supported_with_vdpau(vdpau_supported) {
            Ok(())
        } else {
            Err(format!(
                "decode mode {:?} is not supported on {platform}; supported mode: cpu",
                self.as_str()
            ))
        }
    }

    pub fn validate_current_platform(self) -> Result<(), String> {
        self.validate_for_platform(cfg!(target_os = "freebsd"), std::env::consts::OS)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::VdpauReadback => "vdpau-readback",
            Self::VdpauDirect => "vdpau-direct",
        }
    }
}

impl std::fmt::Display for DecodeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DecodeMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "cpu" => Ok(Self::Cpu),
            "vdpau-readback" => Ok(Self::VdpauReadback),
            "vdpau-direct" => Ok(Self::VdpauDirect),
            _ => Err(format!(
                "invalid decode mode {value:?}; expected cpu, vdpau-readback, or vdpau-direct"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackWakeKind {
    Audio,
    Video,
    Control,
}

impl PlaybackWakeKind {
    const fn bit(self) -> u8 {
        match self {
            Self::Audio => 1 << 0,
            Self::Video => 1 << 1,
            Self::Control => 1 << 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingPlaybackWakes {
    bits: u8,
}

impl PendingPlaybackWakes {
    pub fn contains(self, kind: PlaybackWakeKind) -> bool {
        self.bits & kind.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.bits == 0
    }
}

#[derive(Clone)]
pub struct PlaybackWake {
    pending: Arc<AtomicU8>,
    notify: Arc<dyn Fn() + Send + Sync>,
}

impl PlaybackWake {
    pub fn new(notify: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            pending: Arc::new(AtomicU8::new(0)),
            notify: Arc::new(notify),
        }
    }

    pub fn noop() -> Self {
        Self::new(|| {})
    }

    pub fn wake(&self, kind: PlaybackWakeKind) {
        let previous = self.pending.fetch_or(kind.bit(), Ordering::AcqRel);
        if previous == 0 {
            (self.notify)();
        }
    }

    pub fn take_pending(&self) -> PendingPlaybackWakes {
        PendingPlaybackWakes {
            bits: self.pending.swap(0, Ordering::AcqRel),
        }
    }
}

impl Default for PlaybackWake {
    fn default() -> Self {
        Self::noop()
    }
}

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

    fn next_wake_deadline(&self, _now: Instant) -> Option<Instant> {
        None
    }

    fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::*;

    #[test]
    fn playback_wakes_coalesce_until_pending_kinds_are_consumed() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let notifications_cb = Arc::clone(&notifications);
        let wake = PlaybackWake::new(move || {
            notifications_cb.fetch_add(1, AtomicOrdering::SeqCst);
        });

        wake.wake(PlaybackWakeKind::Audio);
        wake.wake(PlaybackWakeKind::Video);
        wake.wake(PlaybackWakeKind::Video);
        assert_eq!(notifications.load(AtomicOrdering::SeqCst), 1);

        let pending = wake.take_pending();
        assert!(pending.contains(PlaybackWakeKind::Audio));
        assert!(pending.contains(PlaybackWakeKind::Video));
        assert!(!pending.contains(PlaybackWakeKind::Control));

        wake.wake(PlaybackWakeKind::Control);
        assert_eq!(notifications.load(AtomicOrdering::SeqCst), 2);
        assert!(wake.take_pending().contains(PlaybackWakeKind::Control));
    }
}

#[cfg(test)]
mod decode_mode_tests {
    use super::DecodeMode;

    #[test]
    fn platform_default_is_supported() {
        assert!(DecodeMode::platform_default().is_supported_on_current_platform());
        assert!(DecodeMode::Cpu.is_supported_on_current_platform());
    }

    #[test]
    fn vdpau_request_is_rejected_when_platform_does_not_support_it() {
        let error = DecodeMode::VdpauDirect
            .validate_for_platform(false, "windows")
            .unwrap_err();
        assert_eq!(
            error,
            r#"decode mode "vdpau-direct" is not supported on windows; supported mode: cpu"#
        );
        assert!(
            DecodeMode::Cpu
                .validate_for_platform(false, "windows")
                .is_ok()
        );
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn freebsd_defaults_to_vdpau_direct() {
        assert_eq!(DecodeMode::platform_default(), DecodeMode::VdpauDirect);
        assert!(
            DecodeMode::VdpauReadback
                .validate_current_platform()
                .is_ok()
        );
        assert!(DecodeMode::VdpauDirect.validate_current_platform().is_ok());
    }

    #[cfg(not(target_os = "freebsd"))]
    #[test]
    fn non_freebsd_rejects_vdpau_modes() {
        assert_eq!(DecodeMode::platform_default(), DecodeMode::Cpu);
        assert!(
            DecodeMode::VdpauReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(DecodeMode::VdpauDirect.validate_current_platform().is_err());
    }
}
