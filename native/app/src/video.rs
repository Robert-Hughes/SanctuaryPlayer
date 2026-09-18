use std::fmt;
use std::time::Duration;

use url::Url;

use crate::time_format::parse_friendly_time;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPlatform {
    YouTube,
    Twitch,
}

impl fmt::Display for VideoPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::YouTube => f.write_str("YouTube"),
            Self::Twitch => f.write_str("Twitch"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSource {
    pub platform: VideoPlatform,
    pub id: String,
    pub start_time: Option<Duration>,
}

impl VideoSource {
    pub fn parse(input: &str) -> Result<Self, VideoSourceParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(VideoSourceParseError::Empty);
        }

        if let Some(platform) = platform_for_id(input) {
            return Ok(Self {
                platform,
                id: input.to_owned(),
                start_time: None,
            });
        }

        let url = Url::parse(input).map_err(|_| VideoSourceParseError::Unrecognised)?;
        if url.scheme() == "sanctuaryplayer" {
            return parse_sanctuary_link(&url);
        }

        let host = url
            .host_str()
            .map(|host| host.to_ascii_lowercase())
            .ok_or(VideoSourceParseError::Unrecognised)?;
        if host == "sanctuaryplayer.robdh.uk" {
            return parse_sanctuary_link(&url);
        }

        let (platform, id) = if host == "youtu.be" || host.ends_with(".youtu.be") {
            let id =
                last_nonempty_path_segment(&url).ok_or(VideoSourceParseError::MissingVideoId)?;
            (VideoPlatform::YouTube, id)
        } else if host == "youtube.com" || host.ends_with(".youtube.com") {
            let id = url
                .query_pairs()
                .find_map(|(key, value)| (key == "v").then(|| value.into_owned()))
                .or_else(|| last_nonempty_path_segment(&url))
                .ok_or(VideoSourceParseError::MissingVideoId)?;
            (VideoPlatform::YouTube, id)
        } else if host == "twitch.tv" || host.ends_with(".twitch.tv") {
            let id =
                last_nonempty_path_segment(&url).ok_or(VideoSourceParseError::MissingVideoId)?;
            (VideoPlatform::Twitch, id)
        } else {
            return Err(VideoSourceParseError::UnsupportedHost(host));
        };

        if platform_for_id(&id) != Some(platform) {
            return Err(VideoSourceParseError::InvalidVideoId(id));
        }

        let start_time = parse_start_time(&url, "t")?;

        Ok(Self {
            platform,
            id,
            start_time,
        })
    }
}

fn parse_sanctuary_link(url: &Url) -> Result<VideoSource, VideoSourceParseError> {
    if url.scheme() == "sanctuaryplayer" && url.host_str() != Some("open") {
        return Err(VideoSourceParseError::Unrecognised);
    }

    let id = url
        .query_pairs()
        .find_map(|(key, value)| (key == "videoId").then(|| value.into_owned()))
        .ok_or(VideoSourceParseError::MissingVideoId)?;
    let platform =
        platform_for_id(&id).ok_or_else(|| VideoSourceParseError::InvalidVideoId(id.clone()))?;
    let start_time = parse_start_time(url, "time")?;

    Ok(VideoSource {
        platform,
        id,
        start_time,
    })
}

fn parse_start_time(url: &Url, parameter: &str) -> Result<Option<Duration>, VideoSourceParseError> {
    url.query_pairs()
        .find_map(|(key, value)| (key == parameter).then(|| value.into_owned()))
        .map(|value| {
            parse_friendly_time(&value)
                .ok_or_else(|| VideoSourceParseError::InvalidStartTime(value.clone()))
        })
        .transpose()
}

fn last_nonempty_path_segment(url: &Url) -> Option<String> {
    url.path_segments()?
        .rfind(|part| !part.is_empty())
        .map(str::to_owned)
}

fn platform_for_id(id: &str) -> Option<VideoPlatform> {
    if is_youtube_id(id) {
        Some(VideoPlatform::YouTube)
    } else if is_twitch_id(id) {
        Some(VideoPlatform::Twitch)
    } else {
        None
    }
}

fn is_youtube_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn is_twitch_id(id: &str) -> bool {
    id.len() == 10 && id.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoSourceParseError {
    Empty,
    Unrecognised,
    UnsupportedHost(String),
    MissingVideoId,
    InvalidVideoId(String),
    InvalidStartTime(String),
}

impl fmt::Display for VideoSourceParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("video input is empty"),
            Self::Unrecognised => f.write_str("unrecognised video URL or ID"),
            Self::UnsupportedHost(host) => write!(f, "unsupported video host: {host}"),
            Self::MissingVideoId => f.write_str("video URL does not contain a video ID"),
            Self::InvalidVideoId(id) => write!(f, "invalid video ID: {id}"),
            Self::InvalidStartTime(value) => write!(f, "invalid start time: {value}"),
        }
    }
}

impl std::error::Error for VideoSourceParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_naked_ids() {
        assert_eq!(
            VideoSource::parse("3fgD9k8Hkbc").unwrap(),
            VideoSource {
                platform: VideoPlatform::YouTube,
                id: "3fgD9k8Hkbc".into(),
                start_time: None,
            }
        );
        assert_eq!(
            VideoSource::parse("2395077199").unwrap().platform,
            VideoPlatform::Twitch
        );
    }

    #[test]
    fn parses_supported_youtube_urls_and_times() {
        let cases = [
            ("https://youtu.be/3fgD9k8Hkbc", None),
            ("https://youtu.be/3fgD9k8Hkbc?t=3839", Some(3839)),
            ("https://www.youtube.com/watch?v=3fgD9k8Hkbc", None),
            (
                "https://www.youtube.com/watch?v=3fgD9k8Hkbc&t=54m39s",
                Some(3279),
            ),
            ("https://youtube.com/watch?v=3fgD9k8Hkbc&si=example", None),
        ];
        for (input, seconds) in cases {
            let parsed = VideoSource::parse(input).unwrap();
            assert_eq!(parsed.platform, VideoPlatform::YouTube, "{input}");
            assert_eq!(parsed.id, "3fgD9k8Hkbc", "{input}");
            assert_eq!(
                parsed.start_time.map(|time| time.as_secs()),
                seconds,
                "{input}"
            );
        }
    }

    #[test]
    fn parses_twitch_url_and_time() {
        let parsed =
            VideoSource::parse("https://www.twitch.tv/videos/2386400830?t=1h29m24s").unwrap();
        assert_eq!(parsed.platform, VideoPlatform::Twitch);
        assert_eq!(parsed.id, "2386400830");
        assert_eq!(parsed.start_time.unwrap().as_secs(), 5364);
    }

    #[test]
    fn parses_sanctuary_web_and_custom_links() {
        let cases = [
            (
                "https://sanctuaryplayer.robdh.uk/?videoId=2386400830&time=1h29m24s",
                VideoPlatform::Twitch,
                "2386400830",
                Some(5364),
            ),
            (
                "sanctuaryplayer://open?videoId=3fgD9k8Hkbc&time=54m39s",
                VideoPlatform::YouTube,
                "3fgD9k8Hkbc",
                Some(3279),
            ),
            (
                "sanctuaryplayer://open?videoId=2386400830",
                VideoPlatform::Twitch,
                "2386400830",
                None,
            ),
        ];

        for (input, platform, id, seconds) in cases {
            let parsed = VideoSource::parse(input).unwrap();
            assert_eq!(parsed.platform, platform, "{input}");
            assert_eq!(parsed.id, id, "{input}");
            assert_eq!(
                parsed.start_time.map(|time| time.as_secs()),
                seconds,
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_unknown_or_invalid_sources() {
        assert!(matches!(
            VideoSource::parse("https://example.com/3fgD9k8Hkbc"),
            Err(VideoSourceParseError::UnsupportedHost(_))
        ));
        assert!(matches!(
            VideoSource::parse("https://youtu.be/not-valid"),
            Err(VideoSourceParseError::InvalidVideoId(_))
        ));
        assert!(VideoSource::parse("abc").is_err());
    }
}
