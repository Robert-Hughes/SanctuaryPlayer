use std::sync::Mutex;
use std::time::Duration;

use crate::video::VideoSource;

use super::{MetadataService, PositionService, SavedPosition, VideoMetadata};

#[derive(Default)]
pub struct DummyMetadataService;

impl MetadataService for DummyMetadataService {
    fn metadata_for(&self, source: &VideoSource) -> VideoMetadata {
        let title = match source.platform {
            crate::video::VideoPlatform::Twitch => {
                "FIRST STAND - HLE VS CFO - Game 5 | Dummy Twitch VOD"
            }
            crate::video::VideoPlatform::YouTube => {
                "GAM vs. DRX | Worlds Game 3 | Dummy YouTube video"
            }
        };
        VideoMetadata {
            title: title.into(),
            release_age: Duration::from_secs(4 * 24 * 3600 + 3 * 3600),
        }
    }
}

pub struct DummyPositionService {
    positions: Mutex<Vec<SavedPosition>>,
}

impl Default for DummyPositionService {
    fn default() -> Self {
        Self {
            positions: Mutex::new(vec![
                SavedPosition {
                    source: VideoSource::parse("2386400830").unwrap(),
                    device_id: "Phone".into(),
                    position: Duration::from_secs(6485),
                    modified_age: Duration::from_secs(2 * 60),
                    title: Some("HLE vs CFO | Game 5".into()),
                    release_age: Some(Duration::from_secs(4 * 24 * 3600)),
                },
                SavedPosition {
                    source: VideoSource::parse("2386400830").unwrap(),
                    device_id: "Laptop".into(),
                    position: Duration::from_secs(5771),
                    modified_age: Duration::from_secs(3 * 3600),
                    title: Some("HLE vs CFO | Game 5".into()),
                    release_age: Some(Duration::from_secs(4 * 24 * 3600)),
                },
                SavedPosition {
                    source: VideoSource::parse("3fgD9k8Hkbc").unwrap(),
                    device_id: "Desktop".into(),
                    position: Duration::from_secs(3120),
                    modified_age: Duration::from_secs(2 * 24 * 3600),
                    title: Some("GAM vs. DRX | Worlds Game 3".into()),
                    release_age: Some(Duration::from_secs(12 * 24 * 3600)),
                },
            ]),
        }
    }
}

impl DummyPositionService {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PositionService for DummyPositionService {
    fn positions(&self, _user_id: &str) -> Result<Vec<SavedPosition>, String> {
        Ok(self
            .positions
            .lock()
            .expect("dummy positions mutex poisoned")
            .clone())
    }

    fn save_position(
        &self,
        _user_id: &str,
        device_id: &str,
        source: &VideoSource,
        position: Duration,
    ) -> Result<(), String> {
        let mut positions = self
            .positions
            .lock()
            .expect("dummy positions mutex poisoned");
        if let Some(existing) = positions
            .iter_mut()
            .find(|entry| entry.device_id == device_id && entry.source.id == source.id)
        {
            existing.position = position;
            existing.modified_age = Duration::ZERO;
            return Ok(());
        }
        positions.insert(
            0,
            SavedPosition {
                source: source.clone(),
                device_id: device_id.into(),
                position,
                modified_age: Duration::ZERO,
                title: None,
                release_age: None,
            },
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dummy_metadata_contains_deliberate_spoilers_for_ui_filter_testing() {
        let service = DummyMetadataService;
        let metadata = service.metadata_for(&VideoSource::parse("2386400830").unwrap());
        assert!(metadata.title.contains("Game 5"));
        assert!(metadata.title.contains("VS"));
    }

    #[test]
    fn dummy_positions_can_be_updated_per_device_and_video() {
        let service = DummyPositionService::new();
        let source = VideoSource::parse("2386400830").unwrap();
        service
            .save_position("test-user", "Phone", &source, Duration::from_secs(7000))
            .unwrap();
        let phone = service
            .positions("test-user")
            .unwrap()
            .into_iter()
            .find(|entry| entry.device_id == "Phone" && entry.source.id == source.id)
            .unwrap();
        assert_eq!(phone.position, Duration::from_secs(7000));
        assert_eq!(phone.modified_age, Duration::ZERO);
    }
}
