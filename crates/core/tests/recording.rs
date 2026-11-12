use torrential_core::recording::{MatchStatus, Recording, RecordingMetadata, assess_recording};
fn request(title: &str) -> Recording {
    Recording {
        artist: "AEREA".into(),
        title: title.into(),
        version: None,
        duration_ms: Some(293_333),
        duration_tolerance_ms: 2500,
    }
}
fn candidate(title: &str) -> RecordingMetadata {
    RecordingMetadata { artist: Some("AEREA".into()), title: Some(title.into()), duration_ms: Some(293_333) }
}
#[test]
fn preserves_numeric_recording_identity() {
    let assessment = assess_recording(&request("1000 flores"), &candidate("10.000 Flores"));
    assert_eq!(assessment.status, MatchStatus::Mismatch);
    assert!(assessment.reasons.iter().any(|r| r == "numeric_title_mismatch"));
}
#[test]
fn rejects_live_versions_even_when_names_overlap() {
    assert_eq!(
        assess_recording(&request("1000 flores"), &candidate("1000 flores (LIVE)")).status,
        MatchStatus::Mismatch
    );
}
#[test]
fn requested_remix_must_be_present_and_must_not_be_another_remix() {
    let mut r = request("Isaac");
    r.artist = "Stephan Bodzin".into();
    r.version = Some("Denis Horvat & Kevin de Vries Remix".into());
    let mut c = candidate("Isaac (Denis Horvat & Kevin De Vries Remix)");
    c.artist = Some(r.artist.clone());
    assert_eq!(assess_recording(&r, &c).status, MatchStatus::Compatible);
    c.title = Some("Isaac (Pan-Pot Remix)".into());
    assert_eq!(assess_recording(&r, &c).status, MatchStatus::Mismatch);
    c.title = Some("Isaac".into());
    assert_eq!(assess_recording(&r, &c).status, MatchStatus::Mismatch);
}
#[test]
fn missing_metadata_stays_unknown_and_duration_mismatch_is_explicit() {
    let mut c = candidate("1000 flores");
    c.duration_ms = None;
    assert_eq!(assess_recording(&request("1000 flores"), &c).status, MatchStatus::Unknown);
    c.duration_ms = Some(405_840);
    let a = assess_recording(&request("1000 flores"), &c);
    assert_eq!(a.status, MatchStatus::Mismatch);
    assert!(a.reasons.iter().any(|r| r == "duration_mismatch"));
}
#[test]
fn tolerates_accents_punctuation_artist_in_title_and_small_container_padding() {
    let r = Recording {
        artist: "Kölsch".into(),
        title: "Goldfisch".into(),
        version: None,
        duration_ms: Some(325_723),
        duration_tolerance_ms: 2500,
    };
    let c = RecordingMetadata {
        artist: None,
        title: Some("Kolsch - Goldfisch (Original Mix)".into()),
        duration_ms: Some(327_721),
    };
    assert_eq!(assess_recording(&r, &c).status, MatchStatus::Compatible);
}
#[test]
fn rejects_sequel_titles_and_unrequested_remix() {
    let r = Recording {
        artist: "Tale Of Us".into(),
        title: "Nova".into(),
        version: None,
        duration_ms: None,
        duration_tolerance_ms: 2500,
    };
    for title in ["Tale Of Us Nova Two", "Tale Of Us Nova (Scuba Remix)"] {
        let c = RecordingMetadata { artist: None, title: Some(title.into()), duration_ms: None };
        assert_eq!(assess_recording(&r, &c).status, MatchStatus::Mismatch);
    }
}

#[test]
fn requested_year_qualified_versions_and_numeric_artist_prefixes_are_compatible() {
    let mut r = request("Song");
    r.version = Some("2011 Remaster".into());
    assert_eq!(assess_recording(&r, &candidate("Song (2011 Remaster)")).status, MatchStatus::Compatible);
    r.artist = "808 State".into();
    r.version = None;
    let c = RecordingMetadata { artist: None, title: Some("808 State - Song".into()), duration_ms: r.duration_ms };
    assert_eq!(assess_recording(&r, &c).status, MatchStatus::Compatible);
}
#[test]
fn explicit_artist_credit_conflicts_are_not_hidden_by_artist_names_in_titles() {
    let r = request("Song");
    let mut c = candidate("AEREA - Song");
    c.artist = Some("Other Artist".into());
    let result = assess_recording(&r, &c);
    assert_eq!(result.status, MatchStatus::Mismatch);
    assert!(result.reasons.iter().any(|s| s == "artist_mismatch"));
}
