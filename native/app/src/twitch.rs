//! Twitch VOD playback URL resolution.
//!
//! Twitch does not expose a stable public REST endpoint that maps a VOD ID to its
//! HLS master playlist. The web player first requests a short-lived playback token
//! from Twitch GraphQL, then presents that token to the Usher playlist service.
//! Keep that site-specific extraction detail contained here so the rest of
//! SanctuaryPlayer only deals with ordinary HLS URLs.

use std::fmt;
use std::time::Duration;

use serde_json::Value;
use url::Url;

const TWITCH_GQL_URL: &str = "https://gql.twitch.tv/gql";
const TWITCH_USHER_BASE: &str = "https://usher.ttvnw.net";

// Public client identifier used by Twitch's web-compatible playback flow and by
// yt-dlp's Twitch extractor. It is an identifier, not a client secret.
const TWITCH_CLIENT_ID: &str = "ue6666qo983tsx6so1t0vnawi233wa";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_GQL_RESPONSE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaybackAccessToken {
    value: String,
    signature: String,
}

/// Resolve a Twitch VOD ID to its signed HLS master-playlist URL.
///
/// This performs one blocking GraphQL request. Call it from a media/background
/// worker rather than the winit event thread.
pub fn resolve_vod_m3u8(video_id: &str) -> Result<Url, TwitchVodResolveError> {
    validate_video_id(video_id)?;
    let access = fetch_playback_access_token(video_id)?;
    Ok(build_vod_m3u8_url(video_id, &access))
}

fn validate_video_id(video_id: &str) -> Result<(), TwitchVodResolveError> {
    if video_id.is_empty() || !video_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(TwitchVodResolveError::InvalidVideoId(video_id.to_owned()));
    }
    Ok(())
}

fn fetch_playback_access_token(
    video_id: &str,
) -> Result<PlaybackAccessToken, TwitchVodResolveError> {
    let query = format!(
        r#"{{ videoPlaybackAccessToken(id: "{video_id}", params: {{ platform: "web", playerBackend: "mediaplayer", playerType: "site" }}) {{ value signature }} }}"#
    );
    let body = serde_json::json!({ "query": query }).to_string();

    let config = ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();

    let mut response = agent
        .post(TWITCH_GQL_URL)
        .header("Client-ID", TWITCH_CLIENT_ID)
        .header("Content-Type", "text/plain;charset=UTF-8")
        .send(body.as_bytes())
        .map_err(|error| TwitchVodResolveError::Request(error.to_string()))?;

    let response_body = response
        .body_mut()
        .with_config()
        .limit(MAX_GQL_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|error| TwitchVodResolveError::ResponseRead(error.to_string()))?;

    parse_playback_access_token(&response_body)
}

fn parse_playback_access_token(
    response_body: &str,
) -> Result<PlaybackAccessToken, TwitchVodResolveError> {
    let response: Value = serde_json::from_str(response_body)
        .map_err(|error| TwitchVodResolveError::InvalidResponse(error.to_string()))?;

    let token = response.pointer("/data/videoPlaybackAccessToken");
    let Some(token) = token.filter(|token| !token.is_null()) else {
        let message = response
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Twitch did not return a VOD playback token");
        return Err(TwitchVodResolveError::GraphQl(message.to_owned()));
    };

    let value = token
        .get("value")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            TwitchVodResolveError::InvalidResponse(
                "Twitch playback-token response is missing value".into(),
            )
        })?;
    let signature = token
        .get("signature")
        .and_then(Value::as_str)
        .filter(|signature| !signature.is_empty())
        .ok_or_else(|| {
            TwitchVodResolveError::InvalidResponse(
                "Twitch playback-token response is missing signature".into(),
            )
        })?;

    Ok(PlaybackAccessToken {
        value: value.to_owned(),
        signature: signature.to_owned(),
    })
}

fn build_vod_m3u8_url(video_id: &str, access: &PlaybackAccessToken) -> Url {
    let mut url = Url::parse(&format!("{TWITCH_USHER_BASE}/vod/{video_id}.m3u8"))
        .expect("static Twitch Usher URL must be valid");

    url.query_pairs_mut()
        .append_pair("allow_source", "true")
        .append_pair("allow_audio_only", "true")
        .append_pair("platform", "web")
        .append_pair("player", "twitchweb")
        .append_pair("supported_codecs", "h264")
        .append_pair("playlist_include_framerate", "true")
        .append_pair("sig", &access.signature)
        .append_pair("token", &access.value);

    url
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwitchVodResolveError {
    InvalidVideoId(String),
    Request(String),
    ResponseRead(String),
    InvalidResponse(String),
    GraphQl(String),
}

impl fmt::Display for TwitchVodResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVideoId(id) => write!(f, "invalid Twitch VOD ID: {id}"),
            Self::Request(error) => write!(f, "Twitch playback-token request failed: {error}"),
            Self::ResponseRead(error) => {
                write!(f, "failed to read Twitch playback-token response: {error}")
            }
            Self::InvalidResponse(error) => {
                write!(f, "invalid Twitch playback-token response: {error}")
            }
            Self::GraphQl(error) => write!(f, "Twitch rejected playback-token request: {error}"),
        }
    }
}

impl std::error::Error for TwitchVodResolveError {}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn rejects_non_numeric_vod_ids_without_network_access() {
        assert!(matches!(
            resolve_vod_m3u8("not-a-vod"),
            Err(TwitchVodResolveError::InvalidVideoId(_))
        ));
    }

    #[test]
    fn parses_playback_access_token_response() {
        let response = r#"{
            "data": {
                "videoPlaybackAccessToken": {
                    "value": "{\"expires\":123}",
                    "signature": "0123456789abcdef"
                }
            }
        }"#;

        assert_eq!(
            parse_playback_access_token(response).unwrap(),
            PlaybackAccessToken {
                value: "{\"expires\":123}".into(),
                signature: "0123456789abcdef".into(),
            }
        );
    }

    #[test]
    fn reports_graphql_errors() {
        let response = r#"{
            "data": { "videoPlaybackAccessToken": null },
            "errors": [{ "message": "video unavailable" }]
        }"#;

        assert_eq!(
            parse_playback_access_token(response),
            Err(TwitchVodResolveError::GraphQl("video unavailable".into()))
        );
    }

    #[test]
    fn builds_encoded_h264_master_playlist_url() {
        let url = build_vod_m3u8_url(
            "2845804307",
            &PlaybackAccessToken {
                value: "{\"foo\":\"a b&c\"}".into(),
                signature: "sig+/=".into(),
            },
        );

        assert_eq!(url.host_str(), Some("usher.ttvnw.net"));
        assert_eq!(url.path(), "/vod/2845804307.m3u8");

        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            query.get("supported_codecs").map(String::as_str),
            Some("h264")
        );
        assert_eq!(query.get("sig").map(String::as_str), Some("sig+/="));
        assert_eq!(
            query.get("token").map(String::as_str),
            Some("{\"foo\":\"a b&c\"}")
        );
    }
}
