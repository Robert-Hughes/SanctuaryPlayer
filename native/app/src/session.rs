use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::persistence::write_atomic;
use crate::video::{VideoPlatform, VideoSource};

const SESSION_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionState {
    pub(crate) source: VideoSource,
    pub(crate) position: Duration,
}

impl SessionState {
    pub(crate) fn restore_source(&self) -> VideoSource {
        let mut source = self.source.clone();
        source.start_time = (!self.position.is_zero()).then_some(self.position);
        source
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<Option<SessionState>, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("read session {}: {error}", self.path.display())),
        };
        parse_session(&text)
            .map(Some)
            .map_err(|error| format!("parse session {}: {error}", self.path.display()))
    }

    pub(crate) fn save(&self, state: &SessionState) -> Result<(), String> {
        let platform = match state.source.platform {
            VideoPlatform::YouTube => "youtube",
            VideoPlatform::Twitch => "twitch",
        };
        let position_ms = state.position.as_millis().min(u128::from(u64::MAX)) as u64;
        let value = json!({
            "version": SESSION_VERSION,
            "platform": platform,
            "video_id": state.source.id,
            "position_ms": position_ms,
        });
        let mut text = serde_json::to_string_pretty(&value)
            .map_err(|error| format!("serialise session: {error}"))?;
        text.push('\n');
        write_atomic(&self.path, text.as_bytes())
    }
}

fn parse_session(text: &str) -> Result<SessionState, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "session root was not an object".to_owned())?;

    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "session version was missing or invalid".to_owned())?;
    if version != SESSION_VERSION {
        return Err(format!("unsupported session version {version}"));
    }

    let platform = match object.get("platform").and_then(Value::as_str) {
        Some("youtube") => VideoPlatform::YouTube,
        Some("twitch") => VideoPlatform::Twitch,
        Some(other) => return Err(format!("unknown session platform {other:?}")),
        None => return Err("session platform was missing or invalid".into()),
    };
    let video_id = object
        .get("video_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "session video_id was missing or invalid".to_owned())?;
    let source = VideoSource::parse(video_id)
        .map_err(|error| format!("invalid session video_id {video_id:?}: {error}"))?;
    if source.platform != platform {
        return Err("session platform did not match video_id".into());
    }
    let position_ms = object
        .get("position_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| "session position_ms was missing or invalid".to_owned())?;

    Ok(SessionState {
        source,
        position: Duration::from_millis(position_ms),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_session_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "sanctuary-player-session-test-{}-{unique}",
                std::process::id()
            ))
            .join(name)
    }

    #[test]
    fn missing_session_is_none_and_state_round_trips() {
        let path = temporary_session_path("session.json");
        let store = SessionStore::new(path.clone());
        assert_eq!(store.load().unwrap(), None);

        let expected = SessionState {
            source: VideoSource::parse("2386400830").unwrap(),
            position: Duration::from_millis(3_721_917),
        };
        store.save(&expected).unwrap();
        assert_eq!(store.load().unwrap(), Some(expected));

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn restored_source_carries_saved_time() {
        let state = SessionState {
            source: VideoSource::parse("2386400830").unwrap(),
            position: Duration::from_millis(12_345),
        };
        assert_eq!(
            state.restore_source().start_time,
            Some(Duration::from_millis(12_345))
        );
    }

    #[test]
    fn rejects_unknown_version_and_platform_mismatch() {
        assert!(
            parse_session(
                r#"{"version":2,"platform":"twitch","video_id":"2386400830","position_ms":1}"#
            )
            .is_err()
        );
        assert!(
            parse_session(
                r#"{"version":1,"platform":"youtube","video_id":"2386400830","position_ms":1}"#
            )
            .is_err()
        );
    }
}
