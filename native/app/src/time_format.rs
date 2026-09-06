use std::time::Duration;

/// Parse the human-friendly time forms accepted by the original web player.
pub fn parse_friendly_time(input: &str) -> Option<Duration> {
    let input = input.trim().to_ascii_lowercase();
    if input.is_empty() {
        return None;
    }

    if input.bytes().all(|byte| byte.is_ascii_digit()) {
        return input.parse::<u64>().ok().map(Duration::from_secs);
    }

    if input.contains(':') {
        return parse_colon_time(&input);
    }

    // The web player also accepts a dot as a minutes/seconds separator:
    // `1.45` means 1 minute 45 seconds rather than a decimal value.
    if input.matches('.').count() == 1 && !input.contains(['h', 'm', 's']) {
        let mut parts = input.split('.');
        let minutes = parts.next()?.parse::<u64>().ok()?;
        let seconds = parts.next()?.parse::<u64>().ok()?;
        return minutes
            .checked_mul(60)?
            .checked_add(seconds)
            .map(Duration::from_secs);
    }

    parse_unit_time(&input)
}

fn parse_colon_time(input: &str) -> Option<Duration> {
    let parts: Vec<&str> = input.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in parts {
        values.push(if part.is_empty() {
            0
        } else {
            part.parse::<u64>().ok()?
        });
    }
    let seconds = match values.as_slice() {
        [minutes, seconds] => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] => hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => unreachable!(),
    };
    Some(Duration::from_secs(seconds))
}

fn parse_unit_time(input: &str) -> Option<Duration> {
    let mut chars = input.chars().peekable();
    let mut total = 0_u64;
    let mut saw_value = false;
    let mut default_multiplier = if input.contains('h') {
        3600
    } else if input.contains('m') {
        60
    } else {
        1
    };

    while chars.peek().is_some() {
        let mut digits = String::new();
        while let Some(ch) = chars.peek().copied() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                chars.next();
            } else {
                break;
            }
        }
        if digits.is_empty() {
            return None;
        }
        let value = digits.parse::<u64>().ok()?;
        saw_value = true;
        let multiplier = match chars.peek().copied() {
            Some('h') => {
                chars.next();
                3600
            }
            Some('m') => {
                chars.next();
                60
            }
            Some('s') => {
                chars.next();
                1
            }
            Some('.') => {
                chars.next();
                default_multiplier
            }
            Some(_) => return None,
            None => default_multiplier,
        };
        total = total.checked_add(value.checked_mul(multiplier)?)?;
        default_multiplier = (multiplier / 60).max(1);
    }

    saw_value.then(|| Duration::from_secs(total))
}

pub fn format_friendly_time(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours}h{minutes:02}m{seconds:02}s")
}

pub fn format_colon_time(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{hours}:{minutes:02}:{seconds:02}")
}

pub fn format_relative_position(subject: Duration, relative_to: Duration) -> String {
    let (prefix, delta) = if subject >= relative_to {
        ("+", subject - relative_to)
    } else {
        ("-", relative_to - subject)
    };
    let seconds = delta.as_secs();
    if seconds == 0 {
        return "Same".into();
    }
    if seconds >= 3600 {
        format!("{prefix}{}", plural(seconds / 3600, "hour"))
    } else if seconds >= 60 {
        format!("{prefix}{}", plural(seconds / 60, "minute"))
    } else {
        format!("{prefix}{}", plural(seconds, "second"))
    }
}

pub fn format_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds >= 365 * 24 * 3600 {
        format!("{} ago", plural(seconds / (365 * 24 * 3600), "year"))
    } else if seconds >= 24 * 3600 {
        format!("{} ago", plural(seconds / (24 * 3600), "day"))
    } else if seconds >= 3600 {
        format!("{} ago", plural(seconds / 3600, "hour"))
    } else if seconds >= 60 {
        format!("{} ago", plural(seconds / 60, "minute"))
    } else {
        "Just now".into()
    }
}

fn plural(value: u64, noun: &str) -> String {
    if value == 1 {
        format!("1 {noun}")
    } else {
        format!("{value} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_web_player_time_forms() {
        let cases = [
            ("1234", 1234),
            ("1.45", 105),
            ("4567s", 4567),
            ("1m2s", 62),
            ("2h1m40s", 7300),
            ("2m", 120),
            ("8h20", 8 * 3600 + 20 * 60),
            ("2:34:57", 9297),
            ("3:45", 225),
            (":40", 40),
            ("1:", 60),
        ];
        for (input, expected) in cases {
            assert_eq!(
                parse_friendly_time(input).unwrap().as_secs(),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_invalid_time_forms() {
        for input in ["", "abc", "1::2::3", "1x30"] {
            assert!(parse_friendly_time(input).is_none(), "{input}");
        }
    }

    #[test]
    fn formats_times_and_relative_offsets() {
        let value = Duration::from_secs(3601);
        assert_eq!(format_friendly_time(value), "1h00m01s");
        assert_eq!(format_colon_time(value), "1:00:01");
        assert_eq!(
            format_relative_position(Duration::from_secs(3660), Duration::from_secs(3600)),
            "+1 minute"
        );
        assert_eq!(
            format_relative_position(Duration::from_secs(3598), Duration::from_secs(3600)),
            "-2 seconds"
        );
    }

    #[test]
    fn formats_relative_age() {
        assert_eq!(format_age(Duration::from_secs(20)), "Just now");
        assert_eq!(format_age(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(format_age(Duration::from_secs(2 * 86400)), "2 days ago");
    }
}
