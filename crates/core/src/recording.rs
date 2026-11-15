//! Conservative recording metadata admission, independent of the acquisition transport.
//!
//! `Compatible` means the supplied metadata does not conflict with the request. It is not
//! acoustic identification, a download permission check, or proof of torrent file contents.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recording {
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default = "default_tolerance")]
    pub duration_tolerance_ms: u64,
}
fn default_tolerance() -> u64 {
    2500
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingMetadata {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchStatus {
    Compatible,
    Unknown,
    Mismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingAssessment {
    pub status: MatchStatus,
    pub reasons: Vec<String>,
}

/// Compares supplied artist/title/version/duration evidence. Missing requested evidence stays
/// unknown; any observed conflict takes precedence over unknown evidence.
pub fn assess_recording(wanted: &Recording, actual: &RecordingMetadata) -> RecordingAssessment {
    let mut conflicts = Vec::new();
    let mut unknown = Vec::new();
    let artist = tokens(&wanted.artist);
    let title = tokens(&wanted.title);
    let version = tokens(wanted.version.as_deref().unwrap_or_default());
    if artist.is_empty() || title.is_empty() {
        unknown.push("empty_request");
    }
    let actual_title = tokens(actual.title.as_deref().unwrap_or_default());
    let actual_artist = tokens(actual.artist.as_deref().unwrap_or_default());
    if actual_title.is_empty() {
        unknown.push("missing_title");
    } else {
        if !title.is_subset(&actual_title) {
            conflicts.push("title_mismatch");
        }
        let mut qualified_title = title.clone();
        qualified_title.extend(version.iter().cloned());
        let expected_numbers = numbers(&qualified_title);
        let actual_numbers = numbers(&actual_title);
        let artist_numbers = numbers(&artist);
        if !expected_numbers.is_subset(&actual_numbers)
            || actual_numbers.difference(&expected_numbers).any(|n| !artist_numbers.contains(n))
        {
            conflicts.push("numeric_title_mismatch");
        }
        if !version.is_subset(&actual_title) {
            conflicts.push("version_mismatch");
        }
        let mut expected = title.clone();
        expected.extend(artist.iter().cloned());
        expected.extend(version);
        for variant in [
            "live",
            "mixed",
            "remix",
            "edit",
            "extended",
            "cover",
            "karaoke",
            "remastered",
            "remaster",
            "sped",
            "slowed",
            "synthapella",
        ] {
            if actual_title.contains(variant) && !expected.contains(variant) {
                conflicts.push("unrequested_version");
                break;
            }
        }
        let qualifiers = [
            "original",
            "mix",
            "official",
            "audio",
            "video",
            "music",
            "visualizer",
            "visualiser",
            "techno",
            "hd",
            "hq",
            "feat",
            "ft",
            "topic",
        ];
        if actual_title.difference(&expected).any(|t| !qualifiers.contains(&t.as_str())) {
            conflicts.push("additional_title_qualifier");
        }
    }
    if actual_artist.is_empty() {
        if !artist.is_subset(&actual_title) {
            unknown.push("missing_artist");
        }
    } else if !artist.is_subset(&actual_artist) {
        conflicts.push("artist_mismatch");
    }
    if let Some(expected) = wanted.duration_ms {
        match actual.duration_ms {
            None => unknown.push("missing_duration"),
            Some(actual) if actual.abs_diff(expected) > wanted.duration_tolerance_ms => {
                conflicts.push("duration_mismatch")
            }
            Some(_) => {}
        }
    }
    let status = if !conflicts.is_empty() {
        MatchStatus::Mismatch
    } else if !unknown.is_empty() {
        MatchStatus::Unknown
    } else {
        MatchStatus::Compatible
    };
    conflicts.extend(unknown);
    RecordingAssessment { status, reasons: conflicts.into_iter().map(String::from).collect() }
}

fn tokens(text: &str) -> BTreeSet<String> {
    let chars: Vec<_> = text.nfkd().filter(|c| !is_combining_mark(*c)).flat_map(char::to_lowercase).collect();
    let mut normalized = String::new();
    for (i, &ch) in chars.iter().enumerate() {
        if ch.is_alphanumeric() {
            normalized.push(ch);
        } else if matches!(ch, '.' | ',')
            && i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(char::is_ascii_digit)
        {
        } else {
            normalized.push(' ');
        }
    }
    normalized.split_whitespace().filter(|w| *w != "and").map(String::from).collect()
}
fn numbers(tokens: &BTreeSet<String>) -> BTreeSet<&str> {
    tokens.iter().filter(|t| t.chars().all(|c| c.is_ascii_digit())).map(String::as_str).collect()
}
