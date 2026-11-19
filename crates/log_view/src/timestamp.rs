//! GitHub's ISO timestamp at the start of a log line.

/// Milliseconds since the Unix epoch of a timestamp such as
/// `2024-05-01T12:34:56.1234567Z` at the start of `line`, and how many bytes
/// to strip: the timestamp and the one space after it. `None` when the line
/// does not start with one.
pub(crate) fn parse_prefix(line: &str) -> Option<(i64, usize)> {
    let b = line.as_bytes();
    let digits = |from: usize, len: usize| -> Option<i64> {
        let part = b.get(from..from + len)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let separator = |at: usize, expected: u8| (b.get(at) == Some(&expected)).then_some(());
    let year = digits(0, 4)?;
    separator(4, b'-')?;
    let month = digits(5, 2)?;
    separator(7, b'-')?;
    let day = digits(8, 2)?;
    separator(10, b'T')?;
    let hour = digits(11, 2)?;
    separator(13, b':')?;
    let minute = digits(14, 2)?;
    separator(16, b':')?;
    let second = digits(17, 2)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0;
    if b.get(at) == Some(&b'.') {
        let fraction = b[at + 1..]
            .iter()
            .take_while(|d| d.is_ascii_digit())
            .count();
        if fraction == 0 {
            return None;
        }
        millis = b[at + 1..at + 1 + fraction.min(3)]
            .iter()
            .fold(0, |n, d| n * 10 + i64::from(d - b'0'));
        millis *= 10_i64.pow(3 - fraction.min(3) as u32);
        at += 1 + fraction;
    }
    separator(at, b'Z')?;
    at += 1;
    match b.get(at) {
        None => {}
        Some(b' ') => at += 1,
        Some(_) => return None,
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    Some((seconds * 1_000 + millis, at))
}

/// Days from 1970-01-01 to a proleptic Gregorian date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `HH:MM:SS` (UTC) of a time from [`parse_prefix`].
pub(crate) fn clock(millis: i64) -> String {
    let seconds = millis.div_euclid(1_000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_prefix_is_parsed_and_measured() {
        let line = "2024-05-01T12:34:56.1234567Z hello";
        let (millis, len) = parse_prefix(line).expect("a timestamp");
        assert_eq!(&line[len..], "hello");
        assert_eq!(millis, 1_714_566_896_123);
        assert_eq!(clock(millis), "12:34:56");
    }

    #[test]
    fn fraction_is_optional_and_the_epoch_is_zero() {
        assert_eq!(parse_prefix("1970-01-01T00:00:00Z x"), Some((0, 21)));
        assert_eq!(parse_prefix("1970-01-01T00:00:01.5Z"), Some((1_500, 22)));
    }

    #[test]
    fn other_text_is_not_a_timestamp() {
        for line in [
            "",
            "hello",
            "2024-05-01 12:34:56 x",
            "2024-13-01T12:34:56Z x",
            "2024-05-01T25:34:56Z x",
            "2024-05-01T12:34:56.Z x",
            "2024-05-01T12:34:56Zx",
        ] {
            assert_eq!(parse_prefix(line), None, "{line:?}");
        }
    }
}
