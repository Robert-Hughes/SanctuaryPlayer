use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::persistence::write_atomic;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) user_id: Option<String>,
    pub(crate) device_id: Option<String>,
    pub(crate) favourite_qualities: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<Settings, String> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(error) => {
                return Err(format!("read settings {}: {error}", self.path.display()));
            }
        };
        parse_settings(&text)
            .map_err(|error| format!("parse settings {}: {error}", self.path.display()))
    }

    pub(crate) fn save(&self, settings: &Settings) -> Result<(), String> {
        let value = json!({
            "user_id": settings.user_id,
            "device_id": settings.device_id,
            "favourite_qualities": settings.favourite_qualities,
        });
        let mut text = serde_json::to_string_pretty(&value)
            .map_err(|error| format!("serialise settings: {error}"))?;
        text.push('\n');
        write_atomic(&self.path, text.as_bytes())
            .map_err(|error| format!("write settings {}: {error}", self.path.display()))
    }
}

fn parse_settings(text: &str) -> Result<Settings, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "settings root was not an object".to_owned())?;

    let user_id = optional_nonempty_string(object.get("user_id"), "user_id")?;
    let device_id = optional_nonempty_string(object.get("device_id"), "device_id")?;
    if user_id.is_some() != device_id.is_some() {
        return Err("user_id and device_id must either both be present or both be absent".into());
    }
    let favourite_qualities = match object.get("favourite_qualities") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(_) => return Err("favourite_qualities was not a string".into()),
    };

    Ok(Settings {
        user_id,
        device_id,
        favourite_qualities,
    })
}

fn optional_nonempty_string(value: Option<&Value>, field: &str) -> Result<Option<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.clone())),
        Some(Value::String(_)) => Err(format!("{field} was empty")),
        Some(_) => Err(format!("{field} was not a string")),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_settings_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sanctuary-player-settings-test-{}-{unique}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn missing_file_loads_defaults_then_round_trips_settings() {
        let path = temporary_settings_path("roundtrip");
        let store = SettingsStore::new(path.clone());
        assert_eq!(store.load().unwrap(), Settings::default());

        let expected = Settings {
            user_id: Some("test-user".into()),
            device_id: Some("test-device".into()),
            favourite_qualities: "1080p60,720p60".into(),
        };
        store.save(&expected).unwrap();
        assert_eq!(store.load().unwrap(), expected);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn partial_account_is_rejected_instead_of_silently_signing_in() {
        let error = parse_settings(r#"{"user_id":"test-user"}"#).unwrap_err();
        assert!(error.contains("user_id and device_id"));
    }
}
