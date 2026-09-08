use sanctuary_player_app::SanctuaryPlayerApp;
use sanctuary_player_app::video::VideoSource;

const USAGE: &str = "Usage: sanctuary-player [OPTIONS] [VIDEO]\n\n\
VIDEO may be a YouTube/Twitch video ID or URL accepted by SanctuaryPlayer.\n\n\
Options:\n  -v, --video <VIDEO>  Auto-load a video on startup\n      --play, --autoplay  Start playback after the video opens\n  -h, --help           Show this help";

enum CliAction {
    Run {
        initial_video: Option<VideoSource>,
        autoplay: bool,
    },
    Help,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let action = match parse_args(std::env::args().skip(1)) {
        Ok(action) => action,
        Err(error) => {
            eprintln!("error: {error}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    let CliAction::Run {
        initial_video,
        autoplay,
    } = action
    else {
        println!("{USAGE}");
        return Ok(());
    };

    if autoplay && initial_video.is_none() {
        eprintln!("error: --play/--autoplay requires a video argument\n\n{USAGE}");
        std::process::exit(2);
    }

    let event_loop = winit::event_loop::EventLoop::new()?;
    let mut app = match initial_video {
        Some(source) => SanctuaryPlayerApp::with_initial_video_options(source, autoplay),
        None => SanctuaryPlayerApp::new(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut args = args.into_iter();
    let mut video_input = None;
    let mut autoplay = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--play" | "--autoplay" => autoplay = true,
            "-v" | "--video" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{arg} requires a video URL or ID"))?;
                set_video_input(&mut video_input, value)?;
            }
            "--" => {
                for value in args {
                    set_video_input(&mut video_input, value)?;
                }
                break;
            }
            _ if arg.starts_with("--video=") => {
                let value = arg["--video=".len()..].to_owned();
                if value.is_empty() {
                    return Err("--video requires a video URL or ID".into());
                }
                set_video_input(&mut video_input, value)?;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}")),
            _ => set_video_input(&mut video_input, arg)?,
        }
    }

    let initial_video = video_input
        .map(|input| {
            VideoSource::parse(&input)
                .map_err(|error| format!("invalid video argument {input:?}: {error}"))
        })
        .transpose()?;
    Ok(CliAction::Run {
        initial_video,
        autoplay,
    })
}

fn set_video_input(slot: &mut Option<String>, value: String) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err("video may only be specified once".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sanctuary_player_app::video::VideoPlatform;

    use super::*;

    fn parse(values: &[&str]) -> Result<CliAction, String> {
        parse_args(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn accepts_video_option_and_preserves_url_start_time() {
        let CliAction::Run {
            initial_video: Some(source),
            autoplay,
        } = parse(&[
            "--video",
            "https://www.twitch.tv/videos/2386400830?t=1h29m24s",
        ])
        .unwrap()
        else {
            panic!("expected initial video");
        };

        assert!(!autoplay);
        assert_eq!(source.platform, VideoPlatform::Twitch);
        assert_eq!(source.id, "2386400830");
        assert_eq!(source.start_time, Some(Duration::from_secs(5364)));
    }

    #[test]
    fn accepts_positional_video_shorthand() {
        let CliAction::Run {
            initial_video: Some(source),
            autoplay,
        } = parse(&["2395077199"]).unwrap()
        else {
            panic!("expected initial video");
        };
        assert!(!autoplay);
        assert_eq!(source.platform, VideoPlatform::Twitch);
        assert_eq!(source.id, "2395077199");
    }

    #[test]
    fn accepts_autoplay_with_video() {
        let CliAction::Run {
            initial_video: Some(source),
            autoplay,
        } = parse(&["--play", "2395077199"]).unwrap()
        else {
            panic!("expected initial video");
        };
        assert!(autoplay);
        assert_eq!(source.platform, VideoPlatform::Twitch);
    }

    #[test]
    fn rejects_duplicate_or_invalid_video_arguments() {
        assert!(parse(&["--video", "2395077199", "2386400830"]).is_err());
        assert!(parse(&["abc"]).is_err());
        assert!(parse(&["--wat"]).is_err());
    }
}
