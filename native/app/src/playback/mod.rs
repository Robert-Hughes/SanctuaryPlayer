mod dummy;
mod oxideav;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use ::oxideav::core::{FrameLease, VideoColorInfo};
use url::Url;

use crate::model::{DebugInfoSection, PlaybackState, Quality};
use crate::video::VideoSource;

pub use self::oxideav::OxidePlayback;
pub use dummy::DummyPlayback;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecodeMode {
    /// Prefer the platform's hardware path, then software decode.
    ///
    /// Windows currently uses Vulkan Video with CPU readback; Android prefers
    /// direct MediaCodec then MediaCodec readback; FreeBSD prefers VDPAU.
    #[default]
    Auto,
    Cpu,
    VulkanReadback,
    VulkanDirect,
    MediaCodecDirect,
    MediaCodecReadback,
    VdpauReadback,
    VdpauDirect,
}

impl DecodeMode {
    pub const fn platform_default() -> Self {
        Self::Auto
    }

    const fn is_supported_with_backends(
        self,
        vdpau_supported: bool,
        mediacodec_supported: bool,
        vulkan_supported: bool,
    ) -> bool {
        match self {
            Self::Auto | Self::Cpu => true,
            Self::VulkanReadback | Self::VulkanDirect => vulkan_supported,
            Self::MediaCodecDirect | Self::MediaCodecReadback => mediacodec_supported,
            Self::VdpauReadback | Self::VdpauDirect => vdpau_supported,
        }
    }

    pub const fn is_supported_on_current_platform(self) -> bool {
        self.is_supported_with_backends(
            cfg!(target_os = "freebsd"),
            cfg!(target_os = "android"),
            cfg!(target_os = "windows"),
        )
    }

    fn validate_for_platform(
        self,
        vdpau_supported: bool,
        mediacodec_supported: bool,
        vulkan_supported: bool,
        platform: &str,
    ) -> Result<(), String> {
        if self.is_supported_with_backends(vdpau_supported, mediacodec_supported, vulkan_supported)
        {
            return Ok(());
        }

        let mut supported = vec!["auto", "cpu"];
        if vulkan_supported {
            supported.extend(["vulkan-readback", "vulkan-direct"]);
        }
        if mediacodec_supported {
            supported.extend(["mediacodec-readback", "mediacodec-direct"]);
        }
        if vdpau_supported {
            supported.extend(["vdpau-readback", "vdpau-direct"]);
        }
        Err(format!(
            "decode mode {:?} is not supported on {platform}; supported modes: {}",
            self.as_str(),
            supported.join(", ")
        ))
    }

    pub fn validate_current_platform(self) -> Result<(), String> {
        self.validate_for_platform(
            cfg!(target_os = "freebsd"),
            cfg!(target_os = "android"),
            cfg!(target_os = "windows"),
            std::env::consts::OS,
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::VulkanReadback => "vulkan-readback",
            Self::VulkanDirect => "vulkan-direct",
            Self::MediaCodecDirect => "mediacodec-direct",
            Self::MediaCodecReadback => "mediacodec-readback",
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
            "auto" => Ok(Self::Auto),
            "cpu" => Ok(Self::Cpu),
            "vulkan-readback" => Ok(Self::VulkanReadback),
            "vulkan-direct" => Ok(Self::VulkanDirect),
            "mediacodec-direct" => Ok(Self::MediaCodecDirect),
            "mediacodec-readback" => Ok(Self::MediaCodecReadback),
            "vdpau-readback" => Ok(Self::VdpauReadback),
            "vdpau-direct" => Ok(Self::VdpauDirect),
            _ => Err(format!(
                "invalid decode mode {value:?}; expected auto, cpu, vulkan-readback, vulkan-direct, mediacodec-direct, mediacodec-readback, vdpau-readback, or vdpau-direct"
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
    fn intends_playing(&self) -> bool;
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
    fn quality_master_url(&self) -> Option<&Url> {
        None
    }
    fn set_quality(&mut self, quality_id: &str);
    fn update(&mut self, elapsed: Duration);

    fn next_wake_deadline(&self, _now: Instant) -> Option<Instant> {
        None
    }

    fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        None
    }

    fn video_color_info(&self) -> Option<VideoColorInfo> {
        None
    }

    fn debug_info(&self) -> Vec<DebugInfoSection> {
        Vec::new()
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
    fn platform_default_is_automatic() {
        assert_eq!(DecodeMode::platform_default(), DecodeMode::Auto);
        assert!(DecodeMode::Auto.is_supported_on_current_platform());
        assert!(DecodeMode::Cpu.is_supported_on_current_platform());
    }

    #[test]
    fn vdpau_request_is_rejected_without_vdpau() {
        let error = DecodeMode::VdpauDirect
            .validate_for_platform(false, false, false, "windows")
            .unwrap_err();
        assert_eq!(
            error,
            r#"decode mode "vdpau-direct" is not supported on windows; supported modes: auto, cpu"#
        );
    }

    #[test]
    fn mediacodec_modes_require_android_backend() {
        for mode in [DecodeMode::MediaCodecDirect, DecodeMode::MediaCodecReadback] {
            assert!(
                mode.validate_for_platform(false, true, false, "android")
                    .is_ok()
            );
            let error = mode
                .validate_for_platform(false, false, true, "windows")
                .unwrap_err();
            assert!(error.contains("not supported on windows"));
        }
    }

    #[test]
    fn vulkan_modes_require_vulkan_video_backend() {
        for mode in [DecodeMode::VulkanReadback, DecodeMode::VulkanDirect] {
            assert!(
                mode.validate_for_platform(false, false, true, "windows")
                    .is_ok()
            );
            let error = mode
                .validate_for_platform(false, false, false, "linux")
                .unwrap_err();
            assert!(error.contains("not supported on linux"));
        }
    }

    #[test]
    fn automatic_mode_is_valid_with_or_without_hardware_backends() {
        for (vdpau, mediacodec, vulkan) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, true),
        ] {
            assert!(
                DecodeMode::Auto
                    .validate_for_platform(vdpau, mediacodec, vulkan, "test")
                    .is_ok()
            );
        }
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn freebsd_supports_only_vdpau_specific_hardware_modes() {
        assert!(
            DecodeMode::VdpauReadback
                .validate_current_platform()
                .is_ok()
        );
        assert!(DecodeMode::VdpauDirect.validate_current_platform().is_ok());
        assert!(
            DecodeMode::VulkanReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::VulkanDirect
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::MediaCodecDirect
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::MediaCodecReadback
                .validate_current_platform()
                .is_err()
        );
    }

    #[cfg(target_os = "android")]
    #[test]
    fn android_supports_only_mediacodec_specific_hardware_modes() {
        assert!(
            DecodeMode::MediaCodecDirect
                .validate_current_platform()
                .is_ok()
        );
        assert!(
            DecodeMode::MediaCodecReadback
                .validate_current_platform()
                .is_ok()
        );
        assert!(
            DecodeMode::VulkanReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::VulkanDirect
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::VdpauReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(DecodeMode::VdpauDirect.validate_current_platform().is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_supports_vulkan_video_modes() {
        for mode in [DecodeMode::VulkanReadback, DecodeMode::VulkanDirect] {
            assert!(mode.validate_current_platform().is_ok());
        }
        assert!(
            DecodeMode::MediaCodecDirect
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::MediaCodecReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::VdpauReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(DecodeMode::VdpauDirect.validate_current_platform().is_err());
    }

    #[cfg(not(any(target_os = "freebsd", target_os = "android", target_os = "windows")))]
    #[test]
    fn platforms_without_hardware_backend_support_auto_and_cpu_only() {
        assert!(DecodeMode::Auto.validate_current_platform().is_ok());
        assert!(DecodeMode::Cpu.validate_current_platform().is_ok());
        assert!(
            DecodeMode::VulkanReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::VulkanDirect
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::MediaCodecDirect
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::MediaCodecReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(
            DecodeMode::VdpauReadback
                .validate_current_platform()
                .is_err()
        );
        assert!(DecodeMode::VdpauDirect.validate_current_platform().is_err());
    }
}
