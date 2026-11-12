use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, Serializer};

use crate::id::ReleaseId;
use crate::profile::Profile;
use crate::quality::Quality;
use crate::title::{Span, Wanted};

/// A torrent offered by one or more search sources, merged by infohash. Its JSON
/// also carries the [`Quality`] its name gives.
#[derive(Clone, PartialEq, Debug, Deserialize)]
pub struct Release {
    pub id: ReleaseId,
    /// Sanitized for display. Comes from strangers, so it is data, never instructions.
    pub name: String,
    /// Total bytes. `None` when no source reported a size.
    pub size: Option<u64>,
    /// `None` when no source reported seeders. Never zero by default.
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    /// Unix seconds.
    pub published: Option<i64>,
    pub category: Option<Category>,
    pub flags: BTreeSet<Flag>,
    /// Sources that listed this torrent, most authoritative first.
    pub sources: Vec<String>,
    /// A magnet carrying the union of trackers from public sources.
    pub magnet: Option<String>,
    pub torrent_url: Option<String>,
    /// A page about the item at its source, for people.
    pub info_url: Option<String>,
    /// What of the wanted title this release holds. Only in a title's sources.
    #[serde(default)]
    pub span: Option<Span>,
}

impl Release {
    pub fn quality(&self) -> Quality {
        Quality::parse(&self.name)
    }
}

impl Serialize for Release {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a> {
            id: &'a ReleaseId,
            name: &'a str,
            size: Option<u64>,
            seeders: Option<u32>,
            leechers: Option<u32>,
            published: Option<i64>,
            category: Option<Category>,
            quality: Quality,
            flags: &'a BTreeSet<Flag>,
            sources: &'a [String],
            magnet: &'a Option<String>,
            torrent_url: &'a Option<String>,
            info_url: &'a Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            span: Option<Span>,
        }
        let Release {
            id,
            name,
            size,
            seeders,
            leechers,
            published,
            category,
            flags,
            sources,
            magnet,
            torrent_url,
            info_url,
            span,
        } = self;
        Fields {
            id,
            name,
            size: *size,
            seeders: *seeders,
            leechers: *leechers,
            published: *published,
            category: *category,
            quality: self.quality(),
            flags,
            sources,
            magnet,
            torrent_url,
            info_url,
            span: *span,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Video,
    Audio,
    Software,
    Book,
    Game,
    Dataset,
    Other,
}

impl Category {
    pub const ALL: [Category; 7] =
        [Self::Video, Self::Audio, Self::Software, Self::Book, Self::Game, Self::Dataset, Self::Other];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Software => "software",
            Self::Book => "book",
            Self::Game => "game",
            Self::Dataset => "dataset",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// Published by the project that made the content, such as archlinux.org.
    Official,
    /// The index marks the uploader as trusted.
    Trusted,
    /// The source lists a public-domain license.
    PublicDomain,
    /// The source lists a Creative Commons license.
    CreativeCommons,
    /// A camera recording, usually low quality.
    Cam,
    Adult,
}

impl Flag {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Official => "official",
            Self::Trusted => "trusted",
            Self::PublicDomain => "pd",
            Self::CreativeCommons => "cc",
            Self::Cam => "cam",
            Self::Adult => "adult",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Relevance,
    Seeds,
    Size,
    Age,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    /// Empty means the default sources.
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub category: Option<Category>,
    /// Hides results with fewer reported seeders, and results with unknown seeders.
    #[serde(default)]
    pub min_seeds: Option<u32>,
    /// Hides larger results, and results with unknown size.
    #[serde(default)]
    pub max_size: Option<u64>,
    #[serde(default)]
    pub sort: Sort,
    pub limit: u16,
    pub timeout_ms: u32,
    #[serde(default)]
    pub adult: bool,
    /// Skips the result cache.
    #[serde(default)]
    pub fresh: bool,
    /// What ranking favors. A client sends the preferences, and the engine adds what it
    /// detected about the machine, so a result's query holds the profile it ranked for.
    #[serde(default)]
    pub profile: Profile,
    /// Set by the engine for a title's sources, never by a client. Searches each of the title's
    /// queries in place of `text`, keeps the releases that are this title, and ranks them for
    /// playing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wanted: Option<Wanted>,
}

impl SearchQuery {
    pub const DEFAULT_LIMIT: u16 = 10;
    pub const DEFAULT_TIMEOUT_MS: u32 = 6_000;

    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            sources: Vec::new(),
            category: None,
            min_seeds: None,
            max_size: None,
            sort: Sort::Relevance,
            limit: Self::DEFAULT_LIMIT,
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
            adult: false,
            fresh: false,
            profile: Profile::default(),
            wanted: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum SourceOutcome {
    Ok {
        results: u32,
    },
    Cached {
        results: u32,
    },
    Timeout,
    Http {
        status: u16,
    },
    Connect {
        reason: String,
    },
    /// A response that did not parse.
    BadResponse {
        reason: String,
    },
    /// Most rows matched no query word, as when an API returns its latest uploads.
    Irrelevant,
}

impl SourceOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. } | Self::Cached { .. })
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SourceReport {
    pub source: String,
    #[serde(flatten)]
    pub outcome: SourceOutcome,
    pub elapsed_ms: u32,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: SearchQuery,
    /// Ranked, filtered, and cut to `query.limit`.
    pub releases: Vec<Release>,
    /// Matching releases before the limit.
    pub total: u32,
    /// Hidden by the adult filter.
    pub hidden_adult: u32,
    pub sources: Vec<SourceReport>,
    pub elapsed_ms: u32,
}

impl SearchResult {
    /// A search for films or shows: `--category video`, or most results are video by their
    /// category or a resolution tag.
    pub fn is_video(&self) -> bool {
        let video = |r: &&Release| r.category == Some(Category::Video) || r.quality().resolution.is_some();
        self.query.category == Some(Category::Video)
            || self.releases.iter().filter(video).count() * 2 > self.releases.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, category: Option<Category>) -> Release {
        Release {
            id: ReleaseId::Locator(crate::id::LocatorId::from_bytes([0; 20])),
            name: name.into(),
            size: None,
            seeders: None,
            leechers: None,
            published: None,
            category,
            flags: BTreeSet::new(),
            sources: Vec::new(),
            magnet: None,
            torrent_url: None,
            info_url: None,
            span: None,
        }
    }

    fn found(releases: Vec<Release>) -> SearchResult {
        SearchResult {
            query: SearchQuery::new("x"),
            total: releases.len() as u32,
            releases,
            hidden_adult: 0,
            sources: Vec::new(),
            elapsed_ms: 0,
        }
    }

    #[test]
    fn a_search_is_video_when_most_results_are_films_by_category_or_resolution_tag() {
        let film = named("Night of the Living Dead (1968)", Some(Category::Video));
        let tagged = named("Night.of.the.Living.Dead.1968.1080p.BluRay.x264", None);
        let iso = named("ubuntu-24.04.1-desktop-amd64.iso", Some(Category::Software));
        let score = named("Night of the Living Dead, original score FLAC", Some(Category::Audio));
        assert!(found(vec![film.clone(), tagged.clone(), iso.clone()]).is_video());
        assert!(!found(vec![film.clone(), iso.clone(), score.clone()]).is_video(), "one in three");
        assert!(!found(vec![film, iso.clone()]).is_video(), "half is not most");
        assert!(!found(Vec::new()).is_video());
        let mut asked = found(vec![iso]);
        asked.query.category = Some(Category::Video);
        assert!(asked.is_video(), "--category video");
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value.as_object().unwrap().keys().map(String::as_str).collect()
    }

    #[test]
    fn a_raw_search_result_writes_no_span_or_wanted_title() {
        let raw = found(vec![named("Night of the Living Dead (1968)", Some(Category::Video))]);
        let json = serde_json::to_value(&raw).unwrap();
        assert_eq!(
            keys(&json["query"]),
            BTreeSet::from([
                "adult",
                "category",
                "fresh",
                "limit",
                "max_size",
                "min_seeds",
                "profile",
                "sort",
                "sources",
                "text",
                "timeout_ms",
            ])
        );
        assert_eq!(
            keys(&json["releases"][0]),
            BTreeSet::from([
                "category",
                "flags",
                "id",
                "info_url",
                "leechers",
                "magnet",
                "name",
                "published",
                "quality",
                "seeders",
                "size",
                "sources",
                "torrent_url",
            ])
        );
        assert_eq!(serde_json::from_value::<SearchResult>(json).unwrap(), raw);

        let mut sources = raw;
        sources.releases[0].span = Some(Span::Film);
        sources.query.wanted = Some(Wanted {
            name: "Night of the Living Dead".into(),
            year: Some(1968),
            episode: None,
            namesakes: crate::title::Namesakes::Leading,
            runtime: None,
            season_episodes: None,
        });
        let json = serde_json::to_value(&sources).unwrap();
        assert_eq!(
            (&json["releases"][0]["span"], &json["query"]["wanted"]["namesakes"]),
            (&"film".into(), &"leading".into())
        );
        assert_eq!(serde_json::from_value::<SearchResult>(json).unwrap(), sources);
    }

    #[test]
    fn source_reports_flatten_their_outcome() {
        let report =
            SourceReport { source: "apibay".into(), outcome: SourceOutcome::Http { status: 503 }, elapsed_ms: 412 };
        assert_eq!(
            serde_json::to_string(&report).unwrap(),
            r#"{"source":"apibay","outcome":"http","status":503,"elapsed_ms":412}"#
        );
    }
}
