use regex::Regex;

/// Remove title elements that can disclose the outcome/length of an esports series.
pub fn sanitise_title(title: &str) -> String {
    let game = Regex::new(r"(?i)Game\s+\d+").expect("constant regex");
    let versus = Regex::new(r"(?i)[^\s]\s+(vs?\.?)\s+[^\s]").expect("constant regex");
    let mut title = game.replace_all(title, "Game _").into_owned();
    let mut search_from = 0;

    loop {
        let Some(captures) = versus.captures(&title[search_from..]) else {
            break;
        };
        let whole = captures.get(0).expect("whole regex match");
        let marker = captures.get(1).expect("versus capture");
        let match_start = search_from + whole.start();
        let match_end = search_from + whole.end();
        let team_a_end = match_start;
        let team_b_start = match_end - 1;
        let team_a_start = team_name_boundary(&title, team_a_end, -1);
        let team_b_end = team_name_boundary(&title, team_b_start, 1);
        let replacement = format!("_ {} _", marker.as_str());
        title.replace_range(team_a_start..=team_b_end, &replacement);
        search_from = team_a_start + replacement.len();
    }

    title
}

fn team_name_boundary(title: &str, start: usize, direction: i32) -> usize {
    let bytes = title.as_bytes();
    let mut index = start as isize;
    let mut most_recent_non_space = start;
    let mut previous = None;
    let mut words = 0;

    while index >= 0 && (index as usize) < bytes.len() {
        let byte = bytes[index as usize];
        if byte == b' ' && previous != Some(b' ') {
            words += 1;
            if words >= 3 {
                return most_recent_non_space;
            }
        } else if matches!(byte, b'|' | b'/' | b'\\' | b'-') {
            return most_recent_non_space;
        }
        if byte != b' ' {
            most_recent_non_space = index as usize;
        }
        previous = Some(byte);
        index += direction as isize;
    }

    if direction > 0 {
        bytes.len().saturating_sub(1)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_game_number() {
        assert_eq!(sanitise_title("Quarterfinal Game 5"), "Quarterfinal Game _");
    }

    #[test]
    fn hides_team_names_around_versus_markers() {
        let cases = [
            ("GAM vs. DRX | Worlds Game 3", "_ vs. _ | Worlds Game _"),
            ("Team 1 vs. Team 2", "_ vs. _"),
            ("BAG v TAG", "_ v _"),
            ("Today | A A vs B B | Now", "Today | _ vs _ | Now"),
            ("This is a game between A vs B", "This is a _ vs _"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitise_title(input), expected, "{input}");
        }
    }

    #[test]
    fn handles_multiple_matches() {
        assert_eq!(
            sanitise_title("aaaaa vs bbbbbb | cddddd vs dddddd | eeeeee vs ffffff"),
            "_ vs _ | _ vs _ | _ vs _"
        );
    }

    #[test]
    fn leaves_unrelated_text_unchanged() {
        assert_eq!(
            sanitise_title("Nothing to see here!"),
            "Nothing to see here!"
        );
        assert_eq!(
            sanitise_title("Not gonna match chavs is it"),
            "Not gonna match chavs is it"
        );
    }
}
