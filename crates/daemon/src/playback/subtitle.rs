//! Subtitle files as text: their encoding, and SubRip as the WebVTT browsers show.

/// A subtitle file's text as UTF-8. Files that are not UTF-8 are read by their byte-order mark, or
/// else as Windows-1252, the usual encoding of old subtitle files.
pub fn decode(bytes: &[u8]) -> String {
    let utf16 = |decode: fn([u8; 2]) -> u16, bytes: &[u8]| {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|&pair| decode(pair)).collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => utf16(u16::from_le_bytes, rest),
        [0xFE, 0xFF, rest @ ..] => utf16(u16::from_be_bytes, rest),
        _ => match std::str::from_utf8(bytes) {
            Ok(text) => text.to_string(),
            Err(_) => bytes.iter().map(|&byte| windows_1252(byte)).collect(),
        },
    }
}

fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    match byte {
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        _ => char::from(byte),
    }
}

/// WebVTT of a SubRip (`.srt`) file's text, or the text itself when it already is WebVTT. `None`
/// when it holds no cue, as when a server sent an error page in its place.
///
/// SubRip in the wild ends lines with CRLF, leaves out the blank line between cues or after the
/// last one, and writes milliseconds after a `,` or a `.`. Each cue becomes its times as
/// `HH:MM:SS.mmm` and its lines, without the cue number, `<font>` tags, or `{\an8}` placement
/// codes, which WebVTT does not have.
pub fn to_vtt(text: &str) -> Option<String> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n");
    if text.trim_start().starts_with("WEBVTT") {
        return text.contains("-->").then(|| text.trim_start().to_string());
    }
    let mut cues: Vec<((u64, u64), Vec<String>)> = Vec::new();
    for line in text.lines() {
        match times(line) {
            Some(times) => {
                // The line before a cue's times is its number, which the cue before it does not say.
                if let Some((_, said)) = cues.last_mut() {
                    while said.last().is_some_and(|last| last.is_empty()) {
                        said.pop();
                    }
                    said.pop_if(|last| last.bytes().all(|b| b.is_ascii_digit()));
                }
                cues.push((times, Vec::new()));
            }
            None => {
                if let Some((_, said)) = cues.last_mut() {
                    said.push(plain(line));
                }
            }
        }
    }
    if cues.is_empty() {
        return None;
    }
    let mut vtt = String::from("WEBVTT\n");
    for ((start, end), said) in cues {
        vtt.push_str(&format!("\n{} --> {}\n", stamp(start), stamp(end)));
        for line in said.iter().filter(|line| !line.is_empty()) {
            vtt.push_str(line);
            vtt.push('\n');
        }
    }
    Some(vtt)
}

/// The start and end of a cue in milliseconds, from a line such as `00:01:47,250 --> 00:01:50,500`.
fn times(line: &str) -> Option<(u64, u64)> {
    let (start, end) = line.split_once("-->")?;
    // Some files put the cue's position after the end time.
    Some((time(start.trim())?, time(end.split_whitespace().next()?)?))
}

/// `HH:MM:SS,mmm`, `MM:SS.mmm`, and the like, as milliseconds.
fn time(text: &str) -> Option<u64> {
    let (clock, fraction) = text.split_once([',', '.']).unwrap_or((text, "0"));
    let digits = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse::<u64>().ok())?
    };
    let parts: Vec<u64> = clock.split(':').map(digits).collect::<Option<_>>()?;
    let seconds = match parts.as_slice() {
        [hours, minutes, seconds] => hours * 3600 + minutes * 60 + seconds,
        [minutes, seconds] => minutes * 60 + seconds,
        _ => return None,
    };
    let fraction = fraction.get(..3).unwrap_or(fraction);
    let millis = digits(fraction)? * 10u64.pow(3 - fraction.len() as u32);
    Some(seconds * 1000 + millis)
}

fn stamp(millis: u64) -> String {
    let (seconds, millis) = (millis / 1000, millis % 1000);
    format!("{:02}:{:02}:{:02}.{millis:03}", seconds / 3600, seconds / 60 % 60, seconds % 60)
}

/// A cue's line without what WebVTT has no word for, and without an arrow that would read as times.
fn plain(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line.trim();
    while let Some(at) = rest.find(['<', '{']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let font = tail.get(..5).is_some_and(|t| t.eq_ignore_ascii_case("<font"))
            || tail.get(..6).is_some_and(|t| t.eq_ignore_ascii_case("</font"));
        let close = if tail.starts_with("{\\") { tail.find('}') } else { font.then(|| tail.find('>')).flatten() };
        match close {
            Some(close) => rest = &tail[close + 1..],
            None => {
                out.push_str(&tail[..1]);
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out.replace("-->", "->").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_files_become_utf_8_whatever_they_were() {
        assert_eq!(decode("déjà vu".as_bytes()), "déjà vu");
        assert_eq!(decode(b"\xEF\xBB\xBFd\xC3\xA9j\xC3\xA0"), "déjà");
        assert_eq!(decode(b"d\xE9j\xE0 \x93vu\x94 \x85"), "déjà “vu” …");
        assert_eq!(decode(b"\xFF\xFEd\x00\xE9\x00"), "dé");
        assert_eq!(decode(b"\xFE\xFF\x00d\x00\xE9"), "dé");
    }

    #[test]
    fn subrip_becomes_webvtt() {
        let srt = "1\n00:01:47,250 --> 00:01:50,500\nThis blade has a dark past.\n\n\
                   2\n00:01:51,800 --> 00:01:55,800\nIt has shed much innocent blood.\n";
        assert_eq!(
            to_vtt(srt).unwrap(),
            "WEBVTT\n\n00:01:47.250 --> 00:01:50.500\nThis blade has a dark past.\n\n\
             00:01:51.800 --> 00:01:55.800\nIt has shed much innocent blood.\n"
        );
    }

    #[test]
    fn subrip_as_people_write_it_still_converts() {
        // CRLF, a byte-order mark, no blank line between cues or after the last, a dot before the
        // milliseconds, two-part and short times, a position after the times, and styling.
        let srt = "\u{feff}1\r\n00:00:01.5 --> 00:00:02.25  X1:100 X2:200\r\n<font color=\"#fff\">One</font> <i>line</i>\r\n\
                   2\r\n01:02,000 --> 01:03,000\r\n{\\an8}Up top\r\nand --> below\r\n\r\n\r\n\
                   3\r\n1:00:00,000 --> 1:00:01,000\r\n- Two\r\n\r\n- speakers\r\n42\r\n\
                   4\r\n01:00:02,000 --> 01:00:03,000\r\nLast";
        assert_eq!(
            to_vtt(srt).unwrap(),
            "WEBVTT\n\n00:00:01.500 --> 00:00:02.250\nOne <i>line</i>\n\n\
             00:01:02.000 --> 00:01:03.000\nUp top\nand -> below\n\n\
             01:00:00.000 --> 01:00:01.000\n- Two\n- speakers\n42\n\n\
             01:00:02.000 --> 01:00:03.000\nLast\n"
        );
    }

    #[test]
    fn webvtt_passes_through_and_text_without_cues_is_nothing() {
        let vtt = "\u{feff}WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHello\r\n";
        assert_eq!(to_vtt(vtt).unwrap(), "WEBVTT\n\n00:01.000 --> 00:02.000\nHello\n");
        assert_eq!(to_vtt("<html><body>404 Not Found</body></html>"), None);
        assert_eq!(to_vtt("WEBVTT\n\n"), None);
        assert_eq!(to_vtt("1\nsoon --> later\nHello\n"), None);
        assert_eq!(to_vtt(""), None);
    }
}
