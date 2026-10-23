use std::time::Duration;

use torrential_core::Progress;
pub use torrential_core::units::count;
use torrential_core::units::{format_duration, format_rate, format_size};

pub const UNKNOWN: &str = "?";
pub const NONE: &str = "-";

pub fn size(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| UNKNOWN.to_string(), format_size)
}

pub fn percent(progress: &Progress) -> String {
    match progress.size {
        None => UNKNOWN.to_string(),
        Some(size) if progress.verified >= size => "100%".to_string(),
        Some(size) => {
            let tenths = progress.verified as u128 * 1000 / size as u128;
            format!("{}.{}%", tenths / 10, tenths % 10)
        }
    }
}

pub fn rate(bytes_per_sec: u64) -> String {
    format_rate(bytes_per_sec)
}

pub fn eta(seconds: Option<u64>) -> String {
    seconds.map_or_else(|| UNKNOWN.to_string(), |s| format_duration(Duration::from_secs(s)))
}

pub fn elapsed(d: Duration) -> String {
    if d.as_secs() < 600 { format!("{}s", d.as_secs()) } else { format_duration(d) }
}

pub fn seconds_tenths(ms: u32) -> String {
    format!("{}.{}s", ms / 1000, ms % 1000 / 100)
}

pub fn age(published: Option<i64>, now: i64) -> String {
    let Some(published) = published else { return UNKNOWN.to_string() };
    let secs = now.saturating_sub(published).max(0) as u64;
    match secs {
        0..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        86_400..63_072_000 => format!("{}d", secs / 86_400),
        _ => format!("{}y", secs / 31_536_000),
    }
}

pub fn timestamp(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub fn shell_word(word: &str) -> String {
    let plain = !word.is_empty() && word.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./:=@%+,".contains(&b));
    if plain { word.to_string() } else { format!("'{}'", word.replace('\'', r"'\''")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_never_rounds_up_to_done() {
        let p = |verified, size| Progress { verified, size, ..Progress::default() };
        assert_eq!(percent(&p(341, Some(1000))), "34.1%");
        assert_eq!(percent(&p(9_999, Some(10_000))), "99.9%");
        assert_eq!(percent(&p(10_000, Some(10_000))), "100%");
        assert_eq!(percent(&p(0, None)), "?");
    }

    #[test]
    fn ages_step_from_minutes_to_years() {
        let now = 1_760_000_000;
        assert_eq!(age(Some(now - 45 * 60), now), "45m");
        assert_eq!(age(Some(now - 17 * 86_400), now), "17d");
        assert_eq!(age(Some(now - 3 * 365 * 86_400), now), "3y");
        assert_eq!(age(None, now), "?");
    }

    #[test]
    fn timestamps_are_utc_rfc3339() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_791_976_323), "2026-10-14T11:12:03Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn shell_words_quote_only_when_needed() {
        assert_eq!(shell_word("Night.mp4"), "Night.mp4");
        assert_eq!(shell_word("*.mkv"), "'*.mkv'");
        assert_eq!(shell_word("it's here"), r"'it'\''s here'");
    }
}
