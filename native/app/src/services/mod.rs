mod dummy;

use std::time::Duration;

use crate::video::VideoSource;

pub use dummy::{DummyMetadataService, DummyPositionService};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoMetadata {
    pub title: String,
    pub release_age: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPosition {
    pub source: VideoSource,
    pub device_id: String,
    pub position: Duration,
    pub modified_age: Duration,
    pub title: Option<String>,
    pub release_age: Option<Duration>,
}

pub trait MetadataService {
    fn metadata_for(&self, source: &VideoSource) -> VideoMetadata;
}

pub trait PositionService {
    fn positions(&self, user_id: &str) -> Vec<SavedPosition>;
    fn save_position(
        &mut self,
        user_id: &str,
        device_id: &str,
        source: &VideoSource,
        position: Duration,
    );
}
