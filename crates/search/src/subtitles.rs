//! Finds subtitles online for a film or an episode a torrent does not carry subtitles for.
//!
//! The names in the torrent say what the video is. IMDb's title suggestions turn that into an
//! ID, and each [`Provider`] searches by it. A request carries the title, the ID, and for a
//! better match the file's name, size, and hash: nothing about the person or the machine.

mod imdb;
mod opensubtitles;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::Instant;
use torrential_core::title::{Episode, Title};
use torrential_core::{Error, ErrorCode};
use url::Url;

use crate::http::{FetchError, Http};

/// A browser's name, so the lookups say nothing about the program or the person.
const AGENT: &str = "Mozilla/5.0";
const FIND_WITHIN: Duration = Duration::from_secs(8);
const FETCH_WITHIN: Duration = Duration::from_secs(10);
/// A subtitle file is text. A long film's is well under a megabyte.
const LARGEST_SUBTITLE: usize = 2 << 20;
/// Enough of each language to find one whose timing fits.
const PER_LANGUAGE: usize = 5;
/// The names a file may carry are tried in order, and a film has at most a few.
const TITLES_TRIED: usize = 3;
const HASHED: usize = 64 * 1024;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SubtitleConfig {
    /// False keeps every lookup off the network.
    pub online: bool,
    /// Replacement base URLs, for `imdb` and for each provider by name.
    pub endpoints: BTreeMap<String, String>,
}

impl Default for SubtitleConfig {
    fn default() -> Self {
        Self { online: true, endpoints: BTreeMap::new() }
    }
}

/// A video file to find subtitles for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Video {
    /// What the names say the video is, most likely first, as [`torrential_core::title::read`] gives.
    pub titles: Vec<Title>,
    pub file_name: String,
    /// Known once both ends of the file have downloaded.
    pub hash: Option<FileHash>,
}

/// The OpenSubtitles hash of a file, which names the exact release a subtitle was timed for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileHash {
    pub hash: u64,
    pub size: u64,
}

impl FileHash {
    /// The file's size plus its first and last 64 KiB summed as little-endian 64-bit words,
    /// wrapping. `head` and `tail` are those bytes, or the whole of a shorter file.
    pub fn of(size: u64, head: &[u8], tail: &[u8]) -> FileHash {
        let words = |bytes: &[u8]| {
            let bytes = &bytes[..bytes.len().min(HASHED)];
            bytes.as_chunks::<8>().0.iter().fold(0u64, |sum, &word| sum.wrapping_add(u64::from_le_bytes(word)))
        };
        FileHash { hash: size.wrapping_add(words(head)).wrapping_add(words(tail)), size }
    }

    /// How many bytes of each end of a file the hash reads.
    pub const fn reach(size: u64) -> u64 {
        if size < HASHED as u64 { size } else { HASHED as u64 }
    }
}

/// One subtitle a provider offers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Subtitle {
    /// The provider's own ID for it.
    pub id: String,
    /// ISO 639-2, such as `eng`.
    pub language: String,
    /// The subtitle's file or release name.
    pub name: Option<String>,
    /// Matched by the file's hash, so it was timed for this exact file.
    pub exact: bool,
    pub source: &'static str,
    /// Where its text is. This comes from the provider, never from a web page.
    pub url: String,
}

/// What a search found. With no title the names matched nothing on IMDb, and nothing was searched.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Found {
    /// The film or series as IMDb names it, with the episode the file's name gave.
    pub title: Option<Title>,
    /// Each language's subtitles together, exact matches first and then in the provider's order.
    pub subtitles: Vec<Subtitle>,
}

/// What a provider searches for.
pub(crate) struct Wanted<'a> {
    /// Such as `tt1727587`.
    pub imdb: &'a str,
    pub episode: Option<Episode>,
    pub hash: Option<FileHash>,
    pub file_name: &'a str,
}

/// A place that lists subtitles by IMDb ID. Another one, such as the OpenSubtitles API with a
/// person's own key, is one more implementation in [`Online::new`].
pub(crate) trait Provider: Send + Sync {
    fn search<'a>(
        &'a self,
        http: &'a Http,
        wanted: &'a Wanted<'a>,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<Vec<Subtitle>, FetchError>>;
}

pub struct Online {
    http: Http,
    imdb: Url,
    providers: Vec<Box<dyn Provider>>,
    online: bool,
}

impl Online {
    /// Fails with `usage` when an endpoint is not a URL or names nothing known.
    pub fn new(config: &SubtitleConfig) -> Result<Online, Error> {
        let http = Http::named(AGENT, false)
            .map_err(|e| Error::internal(format!("cannot start the HTTP client for subtitles: {e}")))?;
        let known = [imdb::NAME, opensubtitles::NAME];
        if let Some(unknown) = config.endpoints.keys().find(|name| !known.contains(&name.as_str())) {
            return Err(Error::new(ErrorCode::Usage, format!("{unknown} is not a subtitle endpoint"))
                .with_hint(format!("the endpoints are {}", known.join(" and "))));
        }
        let base = |name: &str, default: &str| {
            let url = config.endpoints.get(name).map_or(default, String::as_str);
            let slashed = if url.ends_with('/') { url.to_string() } else { format!("{url}/") };
            Url::parse(&slashed)
                .map_err(|e| Error::new(ErrorCode::Usage, format!("the {name} endpoint {url} is not a URL: {e}")))
        };
        Ok(Online {
            imdb: base(imdb::NAME, imdb::BASE)?,
            providers: vec![Box::new(opensubtitles::OpenSubtitles {
                base: base(opensubtitles::NAME, opensubtitles::BASE)?,
            })],
            http,
            online: config.online,
        })
    }

    /// Subtitles for `video`. `None` when IMDb or every provider failed to answer, so asking
    /// again later can do better. Turned off, it answers at once that it found nothing.
    pub async fn find(&self, video: &Video) -> Option<Found> {
        if !self.online {
            return Some(Found::default());
        }
        let deadline = Instant::now() + FIND_WITHIN;
        let titles = &video.titles[..video.titles.len().min(TITLES_TRIED)];
        let Some(matched) = imdb::find(&self.http, &self.imdb, titles, deadline).await.ok()? else {
            return Some(Found::default());
        };
        let wanted =
            Wanted { imdb: &matched.id, episode: matched.title.episode, hash: video.hash, file_name: &video.file_name };
        let mut answered = false;
        let mut subtitles = Vec::new();
        for provider in &self.providers {
            if let Ok(found) = provider.search(&self.http, &wanted, deadline).await {
                answered = true;
                subtitles.extend(found);
            }
        }
        answered.then(|| Found { title: Some(matched.title), subtitles: best(subtitles) })
    }

    /// The bytes of a subtitle a provider listed.
    pub async fn fetch(&self, subtitle: &Subtitle) -> Result<Vec<u8>, Error> {
        let failed = |reason: String| {
            Error::new(
                ErrorCode::FetchFailed,
                format!("cannot download the subtitle from {}: {reason}", subtitle.source),
            )
            .with_hint("pick another subtitle, or try again in a moment")
        };
        let url = Url::parse(&subtitle.url).ok().filter(|url| matches!(url.scheme(), "https" | "http"));
        let url = url.ok_or_else(|| failed("its address is not a web address".into()))?;
        let fetched = self.http.get(&url, LARGEST_SUBTITLE, Instant::now() + FETCH_WITHIN).await;
        fetched.map_err(|e| failed(crate::fetch_reason(e, FETCH_WITHIN)))
    }
}

/// Each language's subtitles together, in the order the languages first appear. Within a
/// language, exact matches come first and then the provider's order, cut to `PER_LANGUAGE`.
fn best(found: Vec<Subtitle>) -> Vec<Subtitle> {
    let mut languages: Vec<(String, Vec<Subtitle>)> = Vec::new();
    for subtitle in found {
        match languages.iter_mut().find(|(language, _)| *language == subtitle.language) {
            Some((_, group)) => group.push(subtitle),
            None => languages.push((subtitle.language.clone(), vec![subtitle])),
        }
    }
    let ranked = languages.into_iter().flat_map(|(_, mut group)| {
        group.sort_by_key(|subtitle| !subtitle.exact);
        group.truncate(PER_LANGUAGE);
        group
    });
    ranked.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtitle(id: &str, language: &str, exact: bool) -> Subtitle {
        Subtitle { id: id.into(), language: language.into(), name: None, exact, source: "test", url: String::new() }
    }

    #[test]
    fn each_languages_best_come_first() {
        let found = vec![
            subtitle("1", "eng", false),
            subtitle("2", "fre", false),
            subtitle("3", "eng", true),
            subtitle("4", "eng", false),
            subtitle("5", "eng", false),
            subtitle("6", "eng", false),
            subtitle("7", "eng", false),
            subtitle("8", "fre", true),
        ];
        let ids: Vec<String> = best(found).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["3", "1", "4", "5", "6", "8", "2"]);
    }

    #[test]
    fn the_hash_is_the_size_plus_both_ends_as_words() {
        // Eight bytes of 1 are the word 0x0101010101010101.
        let word = 0x0101_0101_0101_0101u64;
        assert_eq!(FileHash::of(16, &[1; 8], &[1; 8]), FileHash { hash: 16 + 2 * word, size: 16 });
        let (head, tail) = (vec![0xff; HASHED + 100], vec![0xff; HASHED]);
        let sum = (0..2 * HASHED / 8).fold(1 << 30, |sum: u64, _| sum.wrapping_add(u64::MAX));
        assert_eq!(FileHash::of(1 << 30, &head, &tail).hash, sum, "only 64 KiB of each end counts, and the sum wraps");
        assert_eq!((FileHash::reach(1000), FileHash::reach(1 << 30)), (1000, 65_536));
    }

    #[test]
    fn an_endpoint_must_be_one_this_knows() {
        let config = |name: &str, url: &str| SubtitleConfig {
            endpoints: [(name.to_string(), url.to_string())].into(),
            ..SubtitleConfig::default()
        };
        assert!(Online::new(&SubtitleConfig::default()).is_ok());
        assert!(Online::new(&config("imdb", "http://127.0.0.1:9")).is_ok());
        let unknown = Online::new(&config("subscene", "http://127.0.0.1:9")).err().unwrap();
        assert_eq!((unknown.code, unknown.message.as_str()), (ErrorCode::Usage, "subscene is not a subtitle endpoint"));
        assert_eq!(Online::new(&config("opensubtitles", "not a url")).err().unwrap().code, ErrorCode::Usage);
    }
}
