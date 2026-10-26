//! Shared policy for preventing automatic display sleep during foreground playback.
use crate::model::PlaybackState;

#[derive(Default)]
pub struct DisplaySleepPolicy {
    enabled: bool,
    foreground: bool,
}

impl DisplaySleepPolicy {
    pub fn set_foreground(&mut self, foreground: bool) {
        self.foreground = foreground;
    }

    pub fn desired(&self, state: &PlaybackState) -> bool {
        self.foreground
            && match state {
                PlaybackState::Playing => true,
                // Do not briefly release inhibition while seeking from Playing.
                PlaybackState::Seeking => self.enabled,
                _ => false,
            }
    }

    pub fn change_needed(&self, state: &PlaybackState) -> Option<bool> {
        let desired = self.desired(state);
        (desired != self.enabled).then_some(desired)
    }

    pub fn applied(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playing_and_seek_keep_display_awake() {
        let mut policy = DisplaySleepPolicy::default();
        assert_eq!(policy.change_needed(&PlaybackState::Playing), None);
        policy.set_foreground(true);
        assert_eq!(policy.change_needed(&PlaybackState::Playing), Some(true));
        policy.applied(true);
        assert_eq!(policy.change_needed(&PlaybackState::Seeking), None);
        assert_eq!(policy.change_needed(&PlaybackState::Paused), Some(false));
        policy.applied(false);
        assert_eq!(policy.change_needed(&PlaybackState::Seeking), None);
        assert_eq!(policy.change_needed(&PlaybackState::Playing), Some(true));
        policy.applied(true);
        policy.set_foreground(false);
        assert_eq!(policy.change_needed(&PlaybackState::Playing), Some(false));
    }
}
/// Acquire or release OS display/system idle inhibition on the event-loop thread.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub fn set_display_sleep_inhibited(enabled: bool) -> Result<(), String> {
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }
    let flags = ES_CONTINUOUS
        | if enabled {
            ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED
        } else {
            0
        };
    if unsafe { SetThreadExecutionState(flags) } == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_display_sleep_inhibited(_enabled: bool) -> Result<(), String> {
    Ok(())
}
