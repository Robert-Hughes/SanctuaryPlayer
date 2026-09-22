//! Resolve a YouTube video ID to the HLS manifest advertised by its player API.
//!
//! This is intentionally limited to discovery. Playback still goes through the
//! same OxideAV HLS path as Twitch, which may not support the selected YouTube
//! rendition (notably its separate audio playlist).

use std::fmt;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};
use url::Url;

const WATCH_BASE: &str = "https://www.youtube.com/watch";
const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
// Matches yt-dlp's visionOS player client at c7fb478. These values are an
// upstream compatibility detail and may need updating as YouTube changes.
const CLIENT_NAME: &str = "VISIONOS";
const CLIENT_VERSION: &str = "1.02";
const CLIENT_ID: &str = "101";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_WATCH_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PLAYER_BYTES: u64 = 2 * 1024 * 1024;

pub fn resolve_vod_m3u8(video_id: &str) -> Result<Url, YoutubeResolveError> {
    validate_video_id(video_id)?;
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();

    let mut watch_url = Url::parse(WATCH_BASE).expect("static YouTube watch URL is valid");
    watch_url.query_pairs_mut().append_pair("v", video_id);
    let mut watch = agent
        .get(watch_url.as_str())
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| YoutubeResolveError::Request(error.to_string()))?;
    let watch_body = watch
        .body_mut()
        .with_config()
        .limit(MAX_WATCH_BYTES)
        .read_to_string()
        .map_err(|error| YoutubeResolveError::ResponseRead(error.to_string()))?;
    let visitor_data = parse_visitor_data(&watch_body)?;

    let body = json!({
        "context": { "client": {
            "clientName": CLIENT_NAME,
            "clientVersion": CLIENT_VERSION,
            "deviceMake": "Apple",
            "deviceModel": "RealityDevice17,1",
            "userAgent": USER_AGENT,
            "osName": "visionOS",
            "osVersion": "26.5.23O471",
            "hl": "en"
        } },
        "videoId": video_id,
        "playbackContext": { "contentPlaybackContext": {
            "html5Preference": "HTML5_PREF_WANTS"
        } },
        "contentCheckOk": true,
        "racyCheckOk": true
    })
    .to_string();
    let mut response = agent
        .post(PLAYER_URL)
        .header("Content-Type", "application/json")
        .header("X-YouTube-Client-Name", CLIENT_ID)
        .header("X-YouTube-Client-Version", CLIENT_VERSION)
        .header("X-Goog-Visitor-Id", &visitor_data)
        .header("Origin", "https://www.youtube.com")
        .header("User-Agent", USER_AGENT)
        .send(body.as_bytes())
        .map_err(|error| YoutubeResolveError::Request(error.to_string()))?;
    let response_body = response
        .body_mut()
        .with_config()
        .limit(MAX_PLAYER_BYTES)
        .read_to_string()
        .map_err(|error| YoutubeResolveError::ResponseRead(error.to_string()))?;
    parse_player_response(&response_body)
}

fn validate_video_id(video_id: &str) -> Result<(), YoutubeResolveError> {
    if video_id.len() != 11
        || !video_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(YoutubeResolveError::InvalidVideoId(video_id.to_owned()));
    }
    Ok(())
}

fn parse_visitor_data(watch_body: &str) -> Result<String, YoutubeResolveError> {
    let pattern = Regex::new(r#""VISITOR_DATA":"((?:\\.|[^"\\])*)""#)
        .expect("static visitor-data regex is valid");
    let encoded = pattern
        .captures(watch_body)
        .and_then(|captures| captures.get(1))
        .ok_or(YoutubeResolveError::MissingVisitorData)?
        .as_str();
    serde_json::from_str::<String>(&format!("\"{encoded}\""))
        .map_err(|error| YoutubeResolveError::InvalidResponse(error.to_string()))
}

fn parse_player_response(response_body: &str) -> Result<Url, YoutubeResolveError> {
    let response: Value = serde_json::from_str(response_body)
        .map_err(|error| YoutubeResolveError::InvalidResponse(error.to_string()))?;
    let status = response
        .pointer("/playabilityStatus/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if status != "OK" {
        let reason = response
            .pointer("/playabilityStatus/reason")
            .and_then(Value::as_str)
            .unwrap_or(status);
        return Err(YoutubeResolveError::Unavailable(reason.to_owned()));
    }
    let raw_url = response
        .pointer("/streamingData/hlsManifestUrl")
        .and_then(Value::as_str)
        .ok_or(YoutubeResolveError::MissingHlsManifest)?;
    let url = Url::parse(raw_url)
        .map_err(|error| YoutubeResolveError::InvalidResponse(error.to_string()))?;
    if url.scheme() != "https" {
        return Err(YoutubeResolveError::InvalidResponse(
            "HLS manifest URL is not HTTPS".into(),
        ));
    }
    Ok(url)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YoutubeResolveError {
    InvalidVideoId(String),
    Request(String),
    ResponseRead(String),
    InvalidResponse(String),
    MissingVisitorData,
    MissingHlsManifest,
    Unavailable(String),
}

impl fmt::Display for YoutubeResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVideoId(id) => write!(f, "invalid YouTube video ID: {id}"),
            Self::Request(error) => write!(f, "YouTube request failed: {error}"),
            Self::ResponseRead(error) => write!(f, "failed to read YouTube response: {error}"),
            Self::InvalidResponse(error) => write!(f, "invalid YouTube response: {error}"),
            Self::MissingVisitorData => f.write_str("YouTube watch page has no visitor data"),
            Self::MissingHlsManifest => f.write_str("YouTube player did not offer an HLS manifest"),
            Self::Unavailable(reason) => write!(f, "YouTube video is unavailable: {reason}"),
        }
    }
}

impl std::error::Error for YoutubeResolveError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_visitor_data_from_watch_page() {
        assert_eq!(
            parse_visitor_data(r#"<script>ytcfg.set({"VISITOR_DATA":"abc\u003d"});</script>"#)
                .unwrap(),
            "abc="
        );
    }

    #[test]
    fn parses_playable_hls_response() {
        let response = r#"{"playabilityStatus":{"status":"OK"},"streamingData":{"hlsManifestUrl":"https://manifest.googlevideo.com/api/manifest/hls_playlist/example"}}"#;
        assert_eq!(
            parse_player_response(response).unwrap().host_str(),
            Some("manifest.googlevideo.com")
        );
    }

    #[test]
    fn reports_missing_hls_and_unavailable_video() {
        assert_eq!(
            parse_player_response(r#"{"playabilityStatus":{"status":"OK"}}"#),
            Err(YoutubeResolveError::MissingHlsManifest)
        );
        assert_eq!(
            parse_player_response(
                r#"{"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in"}}"#
            ),
            Err(YoutubeResolveError::Unavailable("Sign in".into()))
        );
    }

    #[test]
    #[ignore = "requires a live YouTube request"]
    fn resolves_example_video_hls_manifest() {
        let url = resolve_vod_m3u8("TNHNaHOBYG8").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("manifest.googlevideo.com"));
    }
}
