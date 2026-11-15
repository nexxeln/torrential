//! TypeScript for the wire types, which `web/src/wire.ts` holds. Types whose `Serialize` is
//! written by hand say here what they are on the wire.

use ts_rs::{Config, TS};

use crate::catalog::{Country, EpisodeInfoWire};
use crate::error::SkipWire;
use crate::release::ReleaseWire;
use crate::watching::WatchedWire;

/// Implements `TS` for types that serialize as `$ts` by hand.
macro_rules! written_as {
    ($($ty:ty => $ts:literal,)*) => {$(
        impl TS for $ty {
            type WithoutGenerics = Self;
            type OptionInnerType = Self;

            fn name(_: &Config) -> String {
                $ts.to_owned()
            }

            fn inline(_: &Config) -> String {
                $ts.to_owned()
            }
        }
    )*};
}

written_as! {
    crate::TorrentId => "string",
    crate::InfoHash => "string",
    crate::ReleaseId => "string",
    crate::PieceMap => "string",
    crate::TitleId => "string",
    crate::VideoRef => "string",
    crate::title::Span => "string",
    crate::catalog::Countries => "Array<Country>",
    crate::Containers => "Array<Container>",
    Country => "Country",
    crate::Container => "Container",
    crate::Release => "Release",
    crate::Watched => "Watched",
    crate::EpisodeInfo => "EpisodeInfo",
    crate::Skip => "Skip",
}

/// A union of string literals, declared as `name`.
fn words(name: &str, words: impl IntoIterator<Item = &'static str>) -> String {
    let words: Vec<String> = words.into_iter().map(|word| format!("\"{word}\"")).collect();
    format!("type {name} = {};", words.join(" | "))
}

/// Core's wire types, each declared once, in the order `wire.ts` lists them.
pub fn declarations(cfg: &Config) -> Vec<String> {
    use crate::*;
    vec![
        ErrorCode::decl(cfg),
        error::Details::decl(cfg),
        error::Candidate::decl(cfg),
        error::CandidateKind::decl(cfg),
        Error::decl(cfg),
        Category::decl(cfg),
        Flag::decl(cfg),
        Resolution::decl(cfg),
        Codec::decl(cfg),
        Quality::decl(cfg),
        Swarm::decl(cfg),
        ReleaseWire::decl(cfg),
        SourceOutcome::decl(cfg),
        SourceReport::decl(cfg),
        SearchResult::decl(cfg),
        Place::decl(cfg),
        Selection::decl(cfg),
        JobStatus::decl(cfg),
        Progress::decl(cfg),
        TitleLink::decl(cfg),
        Job::decl(cfg),
        protocol::Event::decl(cfg),
        FileRisk::decl(cfg),
        TorrentFile::decl(cfg),
        protocol::MetadataSource::decl(cfg),
        protocol::FilesReply::decl(cfg),
        protocol::ChangeResult::decl(cfg),
        protocol::Kept::decl(cfg),
        protocol::Change::decl(cfg),
        Kind::decl(cfg),
        words("Country", Country::ALL.map(Country::code)),
        TitleCard::decl(cfg),
        Art::decl(cfg),
        EpisodeInfoWire::decl(cfg),
        Season::decl(cfg),
        TitleDetails::decl(cfg),
        WatchedWire::decl(cfg),
        Next::decl(cfg),
        protocol::TitlePage::decl(cfg),
        protocol::Continue::decl(cfg),
        protocol::Home::decl(cfg),
        Check::decl(cfg),
        SkipWire::decl(cfg),
        Tried::decl(cfg),
        AttemptEvent::decl(cfg),
        Playback::decl(cfg),
        Decoding::decl(cfg),
        Decoders::decl(cfg),
        Device::decl(cfg),
        words("Container", Container::ALL.map(Container::name)),
        Tv::decl(cfg),
        CastState::decl(cfg),
        Cast::decl(cfg),
        TvList::decl(cfg),
        CastAction::decl(cfg),
    ]
}
