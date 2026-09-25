mod dummy;
mod oxideav;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use ::oxideav::core::{FrameLease, VideoColorInfo};
use url::Url;

use crate::model::{
    DebugGraph, DebugGraphLane, DebugInfoSection, DebugNode, PlaybackState, Quality,
};
use crate::video::VideoSource;

pub use self::oxideav::OxidePlayback;
pub use dummy::DummyPlayback;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecodeMode {
    /// Prefer the platform's hardware path, then software decode.
    ///
    /// Windows prefers direct Vulkan Video then Vulkan Video readback; Android
    /// prefers direct MediaCodec then MediaCodec readback; FreeBSD prefers VDPAU;
    /// macOS prefers VideoToolbox readback.
    #[default]
    Auto,
    Cpu,
    VideoToolboxReadback,
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

    #[cfg(target_os = "windows")]
    pub(crate) const fn prefers_windows_shared_vulkan_device(self) -> bool {
        matches!(self, Self::Auto | Self::VulkanDirect)
    }

    #[cfg(target_os = "windows")]
    pub(crate) const fn requires_windows_shared_vulkan_device(self) -> bool {
        matches!(self, Self::VulkanDirect)
    }

    const fn is_supported_with_backends(
        self,
        vdpau_supported: bool,
        mediacodec_supported: bool,
        vulkan_supported: bool,
        videotoolbox_supported: bool,
    ) -> bool {
        match self {
            Self::Auto | Self::Cpu => true,
            Self::VideoToolboxReadback => videotoolbox_supported,
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
            cfg!(target_os = "macos"),
        )
    }

    fn validate_for_platform(
        self,
        vdpau_supported: bool,
        mediacodec_supported: bool,
        vulkan_supported: bool,
        videotoolbox_supported: bool,
        platform: &str,
    ) -> Result<(), String> {
        if self.is_supported_with_backends(
            vdpau_supported,
            mediacodec_supported,
            vulkan_supported,
            videotoolbox_supported,
        ) {
            return Ok(());
        }

        let mut supported = vec!["auto", "cpu"];
        if videotoolbox_supported {
            supported.push("videotoolbox-readback");
        }
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
            cfg!(target_os = "macos"),
            std::env::consts::OS,
        )
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::VideoToolboxReadback => "videotoolbox-readback",
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
            "videotoolbox-readback" => Ok(Self::VideoToolboxReadback),
            "vulkan-readback" => Ok(Self::VulkanReadback),
            "vulkan-direct" => Ok(Self::VulkanDirect),
            "mediacodec-direct" => Ok(Self::MediaCodecDirect),
            "mediacodec-readback" => Ok(Self::MediaCodecReadback),
            "vdpau-readback" => Ok(Self::VdpauReadback),
            "vdpau-direct" => Ok(Self::VdpauDirect),
            _ => Err(format!(
                "invalid decode mode {value:?}; expected auto, cpu, videotoolbox-readback, vulkan-readback, vulkan-direct, mediacodec-direct, mediacodec-readback, vdpau-readback, or vdpau-direct"
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
    fn set_volume(&mut self, _volume: f32) {}
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

    fn debug_graph(&self) -> DebugGraph {
        let nodes = self
            .debug_info()
            .into_iter()
            .enumerate()
            .map(|(index, section)| {
                DebugNode::new(
                    format!("playback-section-{index}"),
                    section.title,
                    "",
                    DebugGraphLane::Shared,
                    index as u8,
                    section.rows,
                )
            })
            .collect();
        DebugGraph {
            nodes,
            edges: Vec::new(),
        }
    }
}

static NO_PLAYBACK_STATE: PlaybackState = PlaybackState::Loading;
static NO_PLAYBACK_RATES: [f32; 0] = [];
static NO_PLAYBACK_QUALITIES: [Quality; 0] = [];

impl<P: PlaybackBackend> PlaybackBackend for Option<P> {
    fn open(&mut self, source: &VideoSource) -> Result<(), String> {
        match self.as_mut() {
            Some(playback) => playback.open(source),
            None => Err("no playback backend".into()),
        }
    }

    fn source(&self) -> Option<&VideoSource> {
        self.as_ref().and_then(PlaybackBackend::source)
    }

    fn state(&self) -> &PlaybackState {
        self.as_ref()
            .map(PlaybackBackend::state)
            .unwrap_or(&NO_PLAYBACK_STATE)
    }

    fn intends_playing(&self) -> bool {
        self.as_ref().is_some_and(PlaybackBackend::intends_playing)
    }

    fn play(&mut self) {
        if let Some(playback) = self.as_mut() {
            playback.play();
        }
    }

    fn pause(&mut self) {
        if let Some(playback) = self.as_mut() {
            playback.pause();
        }
    }

    fn position(&self) -> Duration {
        self.as_ref()
            .map(PlaybackBackend::position)
            .unwrap_or(Duration::ZERO)
    }

    fn duration(&self) -> Option<Duration> {
        self.as_ref().and_then(PlaybackBackend::duration)
    }

    fn seek(&mut self, position: Duration) {
        if let Some(playback) = self.as_mut() {
            playback.seek(position);
        }
    }

    fn available_rates(&self) -> &[f32] {
        self.as_ref()
            .map(PlaybackBackend::available_rates)
            .unwrap_or(&NO_PLAYBACK_RATES)
    }

    fn playback_rate(&self) -> f32 {
        self.as_ref()
            .map(PlaybackBackend::playback_rate)
            .unwrap_or(1.0)
    }

    fn set_playback_rate(&mut self, rate: f32) {
        if let Some(playback) = self.as_mut() {
            playback.set_playback_rate(rate);
        }
    }

    fn set_volume(&mut self, volume: f32) {
        if let Some(playback) = self.as_mut() {
            playback.set_volume(volume);
        }
    }

    fn available_qualities(&self) -> &[Quality] {
        self.as_ref()
            .map(PlaybackBackend::available_qualities)
            .unwrap_or(&NO_PLAYBACK_QUALITIES)
    }

    fn quality(&self) -> Option<&Quality> {
        self.as_ref().and_then(PlaybackBackend::quality)
    }

    fn quality_master_url(&self) -> Option<&Url> {
        self.as_ref().and_then(PlaybackBackend::quality_master_url)
    }

    fn set_quality(&mut self, quality_id: &str) {
        if let Some(playback) = self.as_mut() {
            playback.set_quality(quality_id);
        }
    }

    fn update(&mut self, elapsed: Duration) {
        if let Some(playback) = self.as_mut() {
            playback.update(elapsed);
        }
    }

    fn next_wake_deadline(&self, now: Instant) -> Option<Instant> {
        self.as_ref()
            .and_then(|playback| playback.next_wake_deadline(now))
    }

    fn take_video_frame_lease(&mut self) -> Option<FrameLease> {
        self.as_mut()
            .and_then(PlaybackBackend::take_video_frame_lease)
    }

    fn video_color_info(&self) -> Option<VideoColorInfo> {
        self.as_ref().and_then(PlaybackBackend::video_color_info)
    }

    fn debug_info(&self) -> Vec<DebugInfoSection> {
        self.as_ref()
            .map(PlaybackBackend::debug_info)
            .unwrap_or_default()
    }

    fn debug_graph(&self) -> DebugGraph {
        self.as_ref()
            .map(PlaybackBackend::debug_graph)
            .unwrap_or_default()
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
            .validate_for_platform(false, false, false, false, "windows")
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
                mode.validate_for_platform(false, true, false, false, "android")
                    .is_ok()
            );
            let error = mode
                .validate_for_platform(false, false, true, false, "windows")
                .unwrap_err();
            assert!(error.contains("not supported on windows"));
        }
    }

    #[test]
    fn vulkan_modes_require_vulkan_video_backend() {
        for mode in [DecodeMode::VulkanReadback, DecodeMode::VulkanDirect] {
            assert!(
                mode.validate_for_platform(false, false, true, false, "windows")
                    .is_ok()
            );
            let error = mode
                .validate_for_platform(false, false, false, false, "linux")
                .unwrap_err();
            assert!(error.contains("not supported on linux"));
        }
    }

    #[test]
    fn videotoolbox_readback_requires_macos_backend() {
        assert!(
            DecodeMode::VideoToolboxReadback
                .validate_for_platform(false, false, false, true, "macos")
                .is_ok()
        );
        let error = DecodeMode::VideoToolboxReadback
            .validate_for_platform(false, false, false, false, "linux")
            .unwrap_err();
        assert!(error.contains("not supported on linux"));
    }

    #[test]
    fn automatic_mode_is_valid_with_or_without_hardware_backends() {
        for (vdpau, mediacodec, vulkan, videotoolbox) in [
            (false, false, false, false),
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
            (true, true, true, true),
        ] {
            assert!(
                DecodeMode::Auto
                    .validate_for_platform(vdpau, mediacodec, vulkan, videotoolbox, "test")
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

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_auto_prefers_shared_vulkan_while_direct_requires_it() {
        assert!(DecodeMode::Auto.prefers_windows_shared_vulkan_device());
        assert!(!DecodeMode::Auto.requires_windows_shared_vulkan_device());

        assert!(DecodeMode::VulkanDirect.prefers_windows_shared_vulkan_device());
        assert!(DecodeMode::VulkanDirect.requires_windows_shared_vulkan_device());

        assert!(!DecodeMode::VulkanReadback.prefers_windows_shared_vulkan_device());
        assert!(!DecodeMode::VulkanReadback.requires_windows_shared_vulkan_device());
        assert!(!DecodeMode::Cpu.prefers_windows_shared_vulkan_device());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_supports_videotoolbox_readback_only() {
        assert!(
            DecodeMode::VideoToolboxReadback
                .validate_current_platform()
                .is_ok()
        );
        for mode in [
            DecodeMode::VulkanReadback,
            DecodeMode::VulkanDirect,
            DecodeMode::MediaCodecDirect,
            DecodeMode::MediaCodecReadback,
            DecodeMode::VdpauReadback,
            DecodeMode::VdpauDirect,
        ] {
            assert!(mode.validate_current_platform().is_err());
        }
    }
    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "android",
        target_os = "windows",
        target_os = "macos"
    )))]
    #[test]
    fn platforms_without_hardware_backend_support_auto_and_cpu_only() {
        assert!(DecodeMode::Auto.validate_current_platform().is_ok());
        assert!(DecodeMode::Cpu.validate_current_platform().is_ok());
        assert!(
            DecodeMode::VideoToolboxReadback
                .validate_current_platform()
                .is_err()
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
