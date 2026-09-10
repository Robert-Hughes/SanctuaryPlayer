use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use serde_json::Value;
use url::Url;

use crate::video::VideoSource;

use super::{PositionService, SavedPosition};

const DEFAULT_SERVER_BASE: &str = "https://sanctuaryplayer.robdh.uk/";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_POSITIONS_RESPONSE_BYTES: u64 = 256 * 1024;

#[derive(Clone)]
pub struct RemotePositionService {
    base_url: Url,
}

impl Default for RemotePositionService {
    fn default() -> Self {
        Self::new()
    }
}

impl RemotePositionService {
    pub fn new() -> Self {
        Self {
            base_url: Url::parse(DEFAULT_SERVER_BASE).expect("saved-position server URL is valid"),
        }
    }

    #[cfg(test)]
    fn with_base_url(base_url: Url) -> Self {
        Self { base_url }
    }

    fn agent(&self) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        config.into()
    }

    fn endpoint(&self, path: &str) -> Result<Url, String> {
        self.base_url
            .join(path)
            .map_err(|error| format!("build saved-position server URL: {error}"))
    }
}

impl PositionService for RemotePositionService {
    fn positions(&self, user_id: &str) -> Result<Vec<SavedPosition>, String> {
        let mut url = self.endpoint("get-saved-positions")?;
        url.query_pairs_mut().append_pair("user_id", user_id);
        let mut response = self
            .agent()
            .get(url.as_str())
            .call()
            .map_err(|error| format!("get saved positions: {error}"))?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_POSITIONS_RESPONSE_BYTES)
            .read_to_string()
            .map_err(|error| format!("read saved positions: {error}"))?;
        parse_positions(&body)
    }

    fn save_position(
        &self,
        user_id: &str,
        device_id: &str,
        source: &VideoSource,
        position: Duration,
    ) -> Result<(), String> {
        let mut url = self.endpoint("save-position")?;
        let rounded_seconds = position.as_secs_f64().round().clamp(0.0, i64::MAX as f64) as i64;
        url.query_pairs_mut()
            .append_pair("user_id", user_id)
            .append_pair("device_id", device_id)
            .append_pair("video_id", &source.id)
            .append_pair("position", &rounded_seconds.to_string());
        self.agent()
            .post(url.as_str())
            .send_empty()
            .map_err(|error| format!("save position: {error}"))?;
        Ok(())
    }
}

fn parse_positions(body: &str) -> Result<Vec<SavedPosition>, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|error| format!("parse saved-position response: {error}"))?;
    let rows = value
        .as_array()
        .ok_or_else(|| "saved-position response was not an array".to_owned())?;
    let mut positions = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        match parse_position(row) {
            Ok(position) => positions.push(position),
            Err(error) => {
                eprintln!("SanctuaryPlayer: skipping saved-position row {index}: {error}");
            }
        }
    }
    Ok(positions)
}

fn parse_position(value: &Value) -> Result<SavedPosition, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "row was not an object".to_owned())?;
    let video_id = required_string(object.get("video_id"), "video_id")?;
    let source = VideoSource::parse(video_id)
        .map_err(|error| format!("invalid video_id {video_id:?}: {error}"))?;
    let device_id = required_string(object.get("device_id"), "device_id")?.to_owned();
    let seconds = object
        .get("position")
        .and_then(Value::as_i64)
        .ok_or_else(|| "position was not an integer".to_owned())?;
    if seconds < 0 {
        return Err("position was negative".into());
    }
    let modified_age = required_age(object.get("modified_time"), "modified_time")?;
    let title = object
        .get("video_title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let release_age = object
        .get("video_release_date")
        .and_then(Value::as_str)
        .and_then(age_from_rfc3339);

    Ok(SavedPosition {
        source,
        device_id,
        position: Duration::from_secs(seconds as u64),
        modified_age,
        title,
        release_age,
    })
}

fn required_string<'a>(value: Option<&'a Value>, field: &str) -> Result<&'a str, String> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{field} was missing or empty"))
}

fn required_age(value: Option<&Value>, field: &str) -> Result<Duration, String> {
    let timestamp = required_string(value, field)?;
    age_from_rfc3339(timestamp).ok_or_else(|| format!("{field} was not RFC3339"))
}

fn age_from_rfc3339(value: &str) -> Option<Duration> {
    let timestamp_millis = i128::from(DateTime::parse_from_rfc3339(value).ok()?.timestamp_millis());
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i128;
    let age_millis = now_millis.saturating_sub(timestamp_millis);
    if age_millis <= 0 {
        Some(Duration::ZERO)
    } else {
        Some(Duration::from_millis(
            age_millis.min(i128::from(u64::MAX)) as u64
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    #[test]
    fn parses_server_rows_and_skips_unusable_video_ids() {
        let body = r#"[
            {"user_id":"test-user","device_id":"Phone","video_id":"2386400830","position":6485,"modified_time":"2025-01-01T00:00:00+00:00","video_title":"HLE vs CFO","video_release_date":"2025-01-01T00:00:00+00:00"},
            {"user_id":"test-user","device_id":"Old","video_id":"definitely-not-a-video-id","position":10,"modified_time":"2025-01-01T00:00:00+00:00"}
        ]"#;
        let positions = parse_positions(body).unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].source.id, "2386400830");
        assert_eq!(positions[0].device_id, "Phone");
        assert_eq!(positions[0].position, Duration::from_secs(6485));
        assert_eq!(positions[0].title.as_deref(), Some("HLE vs CFO"));
        assert!(positions[0].modified_age > Duration::from_secs(24 * 3600));
        assert!(positions[0].release_age.unwrap() > Duration::from_secs(24 * 3600));
    }

    #[test]
    fn fetch_uses_web_server_query_shape() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]).into_owned();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]")
                .unwrap();
            request
        });
        let service = RemotePositionService::with_base_url(
            Url::parse(&format!("http://{address}/")).unwrap(),
        );
        assert!(service.positions("test user").unwrap().is_empty());
        let request = worker.join().unwrap();
        let first_line = request.lines().next().unwrap();
        assert!(first_line.starts_with("GET /get-saved-positions?"));
        let request_target = first_line.split_whitespace().nth(1).unwrap();
        let url = Url::parse(&format!("http://test{request_target}")).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("user_id").map(String::as_str), Some("test user"));
    }

    #[test]
    fn save_uses_web_server_query_shape_and_whole_seconds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let n = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..n]).into_owned();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
            request
        });
        let service = RemotePositionService::with_base_url(
            Url::parse(&format!("http://{address}/")).unwrap(),
        );
        service
            .save_position(
                "test user",
                "test-device/Desktop",
                &VideoSource::parse("2386400830").unwrap(),
                Duration::from_millis(12_600),
            )
            .unwrap();
        let request = worker.join().unwrap();
        let first_line = request.lines().next().unwrap();
        assert!(first_line.starts_with("POST /save-position?"));
        let request_target = first_line.split_whitespace().nth(1).unwrap();
        let url = Url::parse(&format!("http://test{request_target}")).unwrap();
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("user_id").map(String::as_str), Some("test user"));
        assert_eq!(
            params.get("device_id").map(String::as_str),
            Some("test-device/Desktop")
        );
        assert_eq!(
            params.get("video_id").map(String::as_str),
            Some("2386400830")
        );
        assert_eq!(params.get("position").map(String::as_str), Some("13"));
    }
}
