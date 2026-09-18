#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use sanctuary_player_app::playback::DecodeMode;
use sanctuary_player_app::video::VideoSource;
use sanctuary_player_app::{AppEvent, SanctuaryPlayerApp};

const USAGE: &str = "Usage: sanctuary-player [OPTIONS] [VIDEO]\n\n\
VIDEO may be a YouTube/Twitch video ID or URL, or a sanctuaryplayer:// deep link.\n\n\
Options:\n  -v, --video <VIDEO>       Auto-load a video on startup\n      --play, --autoplay    Start playback after the video opens\n      --mute                Mute audio while keeping the audio playback clock active\n      --decode-mode <MODE>  auto | cpu | vdpau-readback | vdpau-direct (VDPAU: FreeBSD only)\n      --register-uri-handler Register sanctuaryplayer:// for this executable\n  -h, --help                Show this help";

enum CliAction {
    Run {
        initial_video: Option<VideoSource>,
        autoplay: bool,
        muted: bool,
        decode_mode: DecodeMode,
    },
    RegisterUriHandler,
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

    if matches!(action, CliAction::RegisterUriHandler) {
        register_uri_handler()?;
        return Ok(());
    }

    let CliAction::Run {
        initial_video,
        autoplay,
        muted,
        decode_mode,
    } = action
    else {
        println!("{USAGE}");
        return Ok(());
    };

    if autoplay && initial_video.is_none() {
        eprintln!("error: --play/--autoplay requires a video argument\n\n{USAGE}");
        std::process::exit(2);
    }

    let state_dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or("platform state directory is unavailable")?
        .join("sanctuary-player");
    sanctuary_player_app::logging::init(state_dir.join("logs"))?;

    let event_loop = match winit::event_loop::EventLoop::<AppEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            log::error!("SanctuaryPlayer: failed to create event loop: {error}");
            log::logger().flush();
            return Ok(());
        }
    };
    let mut app = match initial_video {
        Some(source) => {
            SanctuaryPlayerApp::with_initial_video_decode_options(source, autoplay, decode_mode)
        }
        None => SanctuaryPlayerApp::with_decode_mode(decode_mode),
    };
    app.set_muted(muted);
    app.set_event_proxy(event_loop.create_proxy());
    app.set_session_path(state_dir.join("session.json"));
    if let Some(config_dir) = dirs::config_dir() {
        app.set_settings_path(config_dir.join("sanctuary-player").join("settings.json"));
    } else {
        log::warn!(
            "SanctuaryPlayer: platform configuration directory is unavailable; settings will not persist"
        );
    }
    if let Err(error) = event_loop.run_app(&mut app) {
        log::error!("SanctuaryPlayer: event loop failed: {error}");
        app.flush_persistence_for_shutdown();
        log::logger().flush();
    }
    Ok(())
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CliAction, String> {
    let mut args = args.into_iter();
    let mut video_input = None;
    let mut autoplay = false;
    let mut decode_mode = DecodeMode::platform_default();
    let mut muted = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--register-uri-handler" => return Ok(CliAction::RegisterUriHandler),
            "--play" | "--autoplay" => autoplay = true,
            "--mute" => muted = true,
            "--decode-mode" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--decode-mode requires a mode".to_owned())?;
                decode_mode = parse_decode_mode(&value)?;
            }
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
            _ if arg.starts_with("--decode-mode=") => {
                let value = &arg["--decode-mode=".len()..];
                if value.is_empty() {
                    return Err("--decode-mode requires a mode".into());
                }
                decode_mode = parse_decode_mode(value)?;
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
        muted,
        decode_mode,
    })
}

fn parse_decode_mode(value: &str) -> Result<DecodeMode, String> {
    let mode: DecodeMode = value.parse()?;
    mode.validate_current_platform()?;
    Ok(mode)
}

fn set_video_input(slot: &mut Option<String>, value: String) -> Result<(), String> {
    if slot.replace(value).is_some() {
        return Err("video may only be specified once".into());
    }
    Ok(())
}

#[cfg(target_os = "freebsd")]
fn register_uri_handler() -> Result<(), Box<dyn std::error::Error>> {
    use std::fs;
    use std::process::Command;

    let executable = std::env::current_exe()?;
    let data_dir = dirs::data_local_dir().ok_or("platform data directory is unavailable")?;
    let applications_dir = data_dir.join("applications");
    fs::create_dir_all(&applications_dir)?;
    let desktop_path = applications_dir.join("sanctuary-player.desktop");

    let quote = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
            .replace('`', "\\`")
    };
    let executable = quote(&executable.to_string_lossy());
    let icon_path = std::env::current_exe()?
        .parent()
        .and_then(|release| release.parent())
        .and_then(|target| target.parent())
        .map(|native| native.join("assets/app-icon.svg"))
        .filter(|path| path.is_file());
    let icon = icon_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sanctuary-player".to_owned());

    let desktop_entry = format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=Sanctuary Player\nComment=Native Twitch and YouTube video player\nExec=\"{executable}\" %u\nIcon={icon}\nTerminal=false\nCategories=AudioVideo;Player;\nKeywords=Sanctuary;Player;Twitch;YouTube;Video;\nMimeType=x-scheme-handler/sanctuaryplayer;\nStartupNotify=false\n"
    );
    fs::write(&desktop_path, desktop_entry)?;

    let status = Command::new("xdg-mime")
        .args([
            "default",
            "sanctuary-player.desktop",
            "x-scheme-handler/sanctuaryplayer",
        ])
        .status()?;
    if !status.success() {
        return Err(format!("xdg-mime failed with status {status}").into());
    }

    println!(
        "Registered sanctuaryplayer:// with {}",
        desktop_path.display()
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn register_uri_handler() -> Result<(), Box<dyn std::error::Error>> {
    use std::process::Command;

    fn reg_add(key: &str, value_name: Option<&str>, value: &str) -> Result<(), String> {
        let mut command = Command::new("reg.exe");
        command.args(["add", key]);
        match value_name {
            Some(name) => {
                command.args(["/v", name]);
            }
            None => {
                command.arg("/ve");
            }
        }
        let output = command
            .args(["/d", value, "/f"])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        Ok(())
    }

    let executable = std::env::current_exe()?;
    let executable = executable.to_string_lossy();
    let root = r"HKCU\Software\Classes\sanctuaryplayer";
    reg_add(root, None, "URL:Sanctuary Player Protocol")?;
    reg_add(root, Some("URL Protocol"), "")?;
    reg_add(
        &format!(r"{root}\DefaultIcon"),
        None,
        &format!("\"{executable}\",0"),
    )?;
    reg_add(
        &format!(r"{root}\shell\open\command"),
        None,
        &format!("\"{executable}\" \"%1\""),
    )?;
    println!("Registered sanctuaryplayer:// for {executable}");
    Ok(())
}

#[cfg(not(any(target_os = "freebsd", target_os = "windows")))]
fn register_uri_handler() -> Result<(), Box<dyn std::error::Error>> {
    Err("URI-handler registration is not implemented for this desktop platform".into())
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
            muted,
            decode_mode,
        } = parse(&[
            "--video",
            "https://www.twitch.tv/videos/2386400830?t=1h29m24s",
        ])
        .unwrap()
        else {
            panic!("expected initial video");
        };

        assert!(!autoplay);
        assert!(!muted);
        assert_eq!(decode_mode, DecodeMode::platform_default());
        assert_eq!(source.platform, VideoPlatform::Twitch);
        assert_eq!(source.id, "2386400830");
        assert_eq!(source.start_time, Some(Duration::from_secs(5364)));
    }

    #[test]
    fn accepts_positional_video_shorthand() {
        let CliAction::Run {
            initial_video: Some(source),
            autoplay,
            muted,
            decode_mode,
        } = parse(&["2395077199"]).unwrap()
        else {
            panic!("expected initial video");
        };
        assert!(!autoplay);
        assert!(!muted);
        assert_eq!(decode_mode, DecodeMode::platform_default());
        assert_eq!(source.platform, VideoPlatform::Twitch);
        assert_eq!(source.id, "2395077199");
    }

    #[test]
    fn accepts_sanctuary_deep_link() {
        let CliAction::Run {
            initial_video: Some(source),
            ..
        } = parse(&["sanctuaryplayer://open?videoId=2386400830&time=1h29m24s"]).unwrap()
        else {
            panic!("expected initial video");
        };

        assert_eq!(source.platform, VideoPlatform::Twitch);
        assert_eq!(source.id, "2386400830");
        assert_eq!(source.start_time, Some(Duration::from_secs(5364)));
    }

    #[test]
    fn accepts_uri_handler_registration_action() {
        assert!(matches!(
            parse(&["--register-uri-handler"]).unwrap(),
            CliAction::RegisterUriHandler
        ));
    }

    #[test]
    fn accepts_autoplay_with_video() {
        let CliAction::Run {
            initial_video: Some(source),
            autoplay,
            muted,
            decode_mode,
        } = parse(&["--play", "2395077199"]).unwrap()
        else {
            panic!("expected initial video");
        };
        assert!(autoplay);
        assert!(!muted);
        assert_eq!(decode_mode, DecodeMode::platform_default());
        assert_eq!(source.platform, VideoPlatform::Twitch);
    }

    #[test]
    fn accepts_mute_without_autoplay() {
        let CliAction::Run {
            autoplay, muted, ..
        } = parse(&["--mute", "2395077199"]).unwrap()
        else {
            panic!("expected run action");
        };
        assert!(!autoplay);
        assert!(muted);
    }
    #[test]
    fn accepts_supported_explicit_decode_modes_and_rejects_unsupported_ones() {
        let CliAction::Run { decode_mode, .. } =
            parse(&["--decode-mode", "cpu", "2395077199"]).unwrap()
        else {
            panic!("expected run action");
        };
        assert_eq!(decode_mode, DecodeMode::Cpu);

        #[cfg(target_os = "freebsd")]
        for (name, expected) in [
            ("vdpau-readback", DecodeMode::VdpauReadback),
            ("vdpau-direct", DecodeMode::VdpauDirect),
        ] {
            let CliAction::Run { decode_mode, .. } =
                parse(&["--decode-mode", name, "2395077199"]).unwrap()
            else {
                panic!("expected run action");
            };
            assert_eq!(decode_mode, expected);
        }

        #[cfg(not(target_os = "freebsd"))]
        for name in ["vdpau-readback", "vdpau-direct"] {
            let error = parse(&["--decode-mode", name, "2395077199"]).unwrap_err();
            assert!(error.contains("not supported"));
        }

        assert!(parse(&["--decode-mode", "banana", "2395077199"]).is_err());
    }

    #[test]
    fn rejects_duplicate_or_invalid_video_arguments() {
        assert!(parse(&["--video", "2395077199", "2386400830"]).is_err());
        assert!(parse(&["abc"]).is_err());
        assert!(parse(&["--wat"]).is_err());
    }
}
