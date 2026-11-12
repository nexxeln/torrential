//! Films and series from Cinemeta, the catalog Stremio uses, read into the core catalog types.
//!
//! Details and pictures are kept under the cache folder, so a title page opens from disk and a
//! picture is fetched once. Picture addresses come only from Cinemeta's answers and never leave
//! this module: callers name a picture by title ID and kind.

mod cinemeta;
mod shelf;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tokio::time::Instant;
use torrential_core::title::normalized;
use torrential_core::{ArtKind, Error, ErrorCode, Kind, TitleCard, TitleDetails, TitleId};
use url::{Host, Origin, Url};

use self::cinemeta::{Described, Listed};
use self::shelf::Shelf;
use crate::http::{FetchError, Http};

const BASE: &str = "https://v3-cinemeta.strem.io/";
/// A browser's name, so the lookups say nothing about the program or the person.
const AGENT: &str = "Mozilla/5.0";
const LOOKUP_WITHIN: Duration = Duration::from_secs(8);
const LARGEST_LISTING: usize = 1 << 20;
/// A long-running series lists thousands of episodes.
const LARGEST_DETAILS: usize = 8 << 20;
const LARGEST_PICTURE: usize = 5 << 20;
const PICTURES_BUDGET: u64 = 256 << 20;
const MISS_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const REDIRECTS: usize = 5;
/// Titles remembered from searches. Past this many the memory starts over.
const LISTED_MOST: usize = 4096;

/// How long a title's details stay fresh: a film rarely changes, and a series gains episodes.
fn fresh_for(kind: Kind) -> Duration {
    match kind {
        Kind::Film => Duration::from_secs(30 * 24 * 60 * 60),
        Kind::Series => Duration::from_secs(24 * 60 * 60),
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CatalogConfig {
    /// False keeps every lookup and every picture fetch off the network.
    pub online: bool,
    /// A Cinemeta mirror. `None` is https://v3-cinemeta.strem.io.
    pub endpoint: Option<String>,
    /// Where details and pictures are kept. `None` keeps them in memory.
    pub cache_dir: Option<PathBuf>,
}

/// A picture of a title, as bytes a browser shows.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Picture {
    pub bytes: Arc<Vec<u8>>,
    pub format: ImageFormat,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
}

impl ImageFormat {
    /// The format `bytes` start like. `None` for anything that is not a JPEG, PNG, or WebP picture.
    pub fn of(bytes: &[u8]) -> Option<ImageFormat> {
        match bytes {
            [0xff, 0xd8, 0xff, ..] => Some(ImageFormat::Jpeg),
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, ..] => Some(ImageFormat::Png),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(ImageFormat::Webp),
            _ => None,
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
            ImageFormat::Webp => "image/webp",
        }
    }
}

/// Why a title has no picture of a kind. Each is remembered for a day.
enum Lack {
    Unnamed,
    Status(u16),
    NotHttps,
    NotImage,
    TooLarge,
}

impl Lack {
    fn error(self, id: &TitleId, kind: ArtKind) -> Error {
        let what = match kind {
            ArtKind::Poster => "poster".to_string(),
            ArtKind::Background => "background".to_string(),
            ArtKind::Logo => "logo".to_string(),
            ArtKind::Thumbnail(e) => format!("season {} episode {} picture", e.season, e.episode),
        };
        let message = match self {
            Lack::Unnamed => format!("Cinemeta names no {what} for {id}"),
            Lack::Status(status) => format!("the {what} for {id} is gone: HTTP {status}"),
            Lack::NotHttps => format!("the {what} for {id} is not at an https address"),
            Lack::NotImage => format!("the {what} for {id} is not a JPEG, PNG, or WebP picture"),
            Lack::TooLarge => format!("the {what} for {id} is larger than {} MiB", LARGEST_PICTURE >> 20),
        };
        Error::new(ErrorCode::NotFound, message)
    }
}

/// Cinemeta, behind the core catalog types.
pub struct Catalog {
    http: Http,
    /// Follows only redirects to an address a picture may come from.
    pictures: Http,
    base: Url,
    online: bool,
    shelf: Shelf,
    /// The kind and poster address of each title recent searches listed.
    listed: Mutex<HashMap<TitleId, (Kind, Option<Url>)>>,
    /// A plain HTTP origin pictures may also come from, for tests against a local server.
    plain: Option<Origin>,
}

impl Catalog {
    /// Fails with `usage` when the endpoint is not a URL.
    pub fn new(config: &CatalogConfig) -> Result<Catalog, Error> {
        let url = config.endpoint.as_deref().unwrap_or(BASE);
        let slashed = if url.ends_with('/') { url.to_string() } else { format!("{url}/") };
        let base = Url::parse(&slashed)
            .map_err(|e| Error::new(ErrorCode::Usage, format!("the catalog endpoint {url} is not a URL: {e}")))?;
        Ok(Catalog {
            http: Http::named(AGENT, false).map_err(no_client)?,
            pictures: picture_client(None)?,
            base,
            online: config.online,
            shelf: Shelf::new(config.cache_dir.clone()),
            listed: Mutex::default(),
            plain: None,
        })
    }

    /// Lets pictures also come over plain HTTP from `origin`, which must be a loopback address
    /// such as a test's local server. Every other picture address must be `https`.
    #[doc(hidden)]
    pub fn allow_plain_http_from(mut self, origin: &str) -> Result<Catalog, Error> {
        let loopback = |url: &Url| match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        let url = Url::parse(origin).ok().filter(|url| url.scheme() == "http" && loopback(url));
        let url =
            url.ok_or_else(|| Error::new(ErrorCode::Usage, format!("{origin} is not a loopback HTTP address")))?;
        self.pictures = picture_client(Some(url.origin()))?;
        self.plain = Some(url.origin());
        Ok(self)
    }

    /// Films and series named like `words`. Titles whose normalized name is the words come first,
    /// then the rest, each group in Cinemeta's order with films and series in turn. So the first of
    /// several same-named titles of a kind is the one Cinemeta lists first. Empty when turned off.
    pub async fn search(&self, words: &str) -> Result<Vec<TitleCard>, Error> {
        if !self.online || words.trim().is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + LOOKUP_WITHIN;
        let (films, series) =
            tokio::join!(self.listing(Kind::Film, words, deadline), self.listing(Kind::Series, words, deadline));
        if let (Err(e), Err(_)) = (&films, &series) {
            return Err(failed("search Cinemeta", e.clone()));
        }
        Ok(ranked(words, films.unwrap_or_default(), series.unwrap_or_default()))
    }

    /// The titles of `card`'s kind under its normalized name, `card` among them, in the order
    /// Cinemeta lists them. Whoever plays `card` tells from this whether another title shares its
    /// name and which one Cinemeta lists first. Empty when turned off.
    pub async fn namesakes(&self, card: &TitleCard) -> Result<Vec<TitleCard>, Error> {
        if !self.online {
            return Ok(Vec::new());
        }
        let listed = self.listing(card.kind, &card.name, Instant::now() + LOOKUP_WITHIN).await;
        let name = normalized(&card.name);
        let namesakes = listed.map_err(|e| failed("search Cinemeta", e))?.into_iter();
        Ok(namesakes.filter(|other| other.kind == card.kind && normalized(&other.name) == name).collect())
    }

    /// From disk while fresh, a film for 30 days and a series for 1 day. A stale copy answers
    /// when Cinemeta does not. Turned off, only a kept copy answers.
    pub async fn title(&self, id: &TitleId) -> Result<TitleDetails, Error> {
        Ok(self.described(id).await?.details)
    }

    /// A picture Cinemeta named for this title, fetched once and kept. Only from an `https`
    /// address, at most 5 MiB, and only bytes that start like JPEG, PNG, or WebP.
    pub async fn art(&self, id: &TitleId, kind: ArtKind) -> Result<Picture, Error> {
        let name = picture_name(id, kind);
        if let Some(kept) = self.shelf.get(&name).await
            && let Some(format) = ImageFormat::of(&kept.bytes)
        {
            self.shelf.touch(&name).await;
            return Ok(Picture { bytes: kept.bytes, format });
        }
        let missed = format!("{name}.miss");
        if let Some(miss) = self.shelf.get(&missed).await
            && age(miss.at) < MISS_FOR
        {
            return Err(Error::new(ErrorCode::NotFound, String::from_utf8_lossy(&miss.bytes)));
        }
        if !self.online {
            return Err(turned_off(&format!("the picture for {id} is not kept on this machine")));
        }
        let lack = match self.address(id, kind).await? {
            None => Lack::Unnamed,
            Some(url) if !may_fetch(&url, self.plain.as_ref()) => Lack::NotHttps,
            Some(url) => match self.pictures.get(&url, LARGEST_PICTURE, Instant::now() + LOOKUP_WITHIN).await {
                Ok(bytes) => match ImageFormat::of(&bytes) {
                    Some(format) => {
                        self.shelf.put(&name, &bytes).await;
                        self.shelf.trim(shelf::PICTURES, PICTURES_BUDGET).await;
                        return Ok(Picture { bytes: Arc::new(bytes), format });
                    }
                    None => Lack::NotImage,
                },
                Err(FetchError::TooLarge(_)) => Lack::TooLarge,
                Err(FetchError::Status(status)) if (300..400).contains(&status) => Lack::NotHttps,
                Err(FetchError::Status(status)) if (400..500).contains(&status) => Lack::Status(status),
                Err(e) => return Err(failed(&format!("fetch a picture for {id}"), e)),
            },
        };
        let error = lack.error(id, kind);
        self.shelf.put(&missed, error.message.as_bytes()).await;
        Err(error)
    }

    /// Cinemeta's titles of `kind` for `words`, asked in lowercase so the same words make the same request.
    async fn listing(&self, kind: Kind, words: &str, deadline: Instant) -> Result<Vec<TitleCard>, FetchError> {
        let words = words.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let query = utf8_percent_encode(&words, NON_ALPHANUMERIC);
        let path = format!("catalog/{}/top/search={query}.json", cinemeta::segment(kind));
        let url = self.base.join(&path).map_err(|e| FetchError::Connect(e.to_string()))?;
        let body = self.http.get(&url, LARGEST_LISTING, deadline).await?;
        let listed = cinemeta::listing(&body, kind).map_err(|e| FetchError::Decode(e.to_string()))?;
        let mut remembered = lock(&self.listed);
        if remembered.len() + listed.len() > LISTED_MOST {
            remembered.clear();
        }
        let cards = listed.into_iter().map(|Listed { card, poster }| {
            remembered.insert(card.id.clone(), (card.kind, poster));
            card
        });
        Ok(cards.collect())
    }

    async fn described(&self, id: &TitleId) -> Result<Described, Error> {
        let kept = match self.kept(id).await {
            Some((age, described)) if age < fresh_for(described.details.card.kind) => return Ok(described),
            kept => kept.map(|(_, described)| described),
        };
        if !self.online {
            return kept.ok_or_else(|| turned_off(&format!("{id} is not kept on this machine")));
        }
        let kind = kept.as_ref().map(|d| d.details.card.kind).or_else(|| self.listed(id).map(|(kind, _)| kind));
        match self.fetch_described(id, kind).await {
            Ok(Some((body, described))) => {
                self.shelf.put(&details_name(id), &body).await;
                Ok(described)
            }
            Ok(None) => {
                kept.ok_or_else(|| Error::new(ErrorCode::NotFound, format!("Cinemeta has no film or series {id}")))
            }
            Err(e) => kept.ok_or_else(|| failed(&format!("look up {id} on Cinemeta"), e)),
        }
    }

    /// The kept copy of a title's details and its age.
    async fn kept(&self, id: &TitleId) -> Option<(Duration, Described)> {
        let kept = self.shelf.get(&details_name(id)).await?;
        Some((age(kept.at), cinemeta::described(&kept.bytes, id).ok().flatten()?))
    }

    /// Asks Cinemeta for `id` as `kind`, or when the kind is not known as a series and then as a
    /// film. Asked first as a film, Cinemeta answers some series with another film's details.
    async fn fetch_described(
        &self,
        id: &TitleId,
        kind: Option<Kind>,
    ) -> Result<Option<(Vec<u8>, Described)>, FetchError> {
        let kinds = kind.map_or_else(|| vec![Kind::Series, Kind::Film], |kind| vec![kind]);
        let deadline = Instant::now() + LOOKUP_WITHIN;
        for kind in kinds {
            let path = format!("meta/{}/{id}.json", cinemeta::segment(kind));
            let url = self.base.join(&path).map_err(|e| FetchError::Connect(e.to_string()))?;
            let body = match self.http.get(&url, LARGEST_DETAILS, deadline).await {
                Err(FetchError::Status(404)) => continue,
                body => body?,
            };
            if let Some(described) = cinemeta::described(&body, id).map_err(|e| FetchError::Decode(e.to_string()))? {
                return Ok(Some((body, described)));
            }
        }
        Ok(None)
    }

    /// Where Cinemeta says a picture is: a poster a search named, or else what the title's details
    /// name. A stale copy of the details names pictures well enough.
    async fn address(&self, id: &TitleId, kind: ArtKind) -> Result<Option<Url>, Error> {
        if kind == ArtKind::Poster
            && let Some((_, Some(poster))) = self.listed(id)
        {
            return Ok(Some(poster));
        }
        let described = match self.kept(id).await {
            Some((_, described)) => described,
            None => self.described(id).await?,
        };
        Ok(described.pictures.address(kind).cloned())
    }

    fn listed(&self, id: &TitleId) -> Option<(Kind, Option<Url>)> {
        lock(&self.listed).get(id).cloned()
    }
}

/// Titles whose normalized name is `words` first, then the rest. Each group keeps Cinemeta's
/// order, taking films and series in turn. A title listed twice stays where it first appears.
fn ranked(words: &str, films: Vec<TitleCard>, series: Vec<TitleCard>) -> Vec<TitleCard> {
    let wanted = normalized(words);
    let turns = (0..films.len().max(series.len())).flat_map(|i| [films.get(i), series.get(i)]).flatten();
    let mut seen = HashSet::new();
    let mut ranked: Vec<TitleCard> = turns.filter(|card| seen.insert(card.id.clone())).cloned().collect();
    ranked.sort_by_key(|card| normalized(&card.name) != wanted);
    ranked
}

fn may_fetch(url: &Url, plain: Option<&Origin>) -> bool {
    url.scheme() == "https" || plain.is_some_and(|origin| url.origin() == *origin)
}

/// A client for pictures. A redirect to an address a picture may not come from stops, and its
/// answer then reads as a failed fetch.
fn picture_client(plain: Option<Origin>) -> Result<Http, Error> {
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= REDIRECTS {
            attempt.error("too many redirects")
        } else if may_fetch(attempt.url(), plain.as_ref()) {
            attempt.follow()
        } else {
            attempt.stop()
        }
    });
    Http::redirecting(AGENT, false, policy).map_err(no_client)
}

fn details_name(id: &TitleId) -> String {
    format!("{}/{id}.json", shelf::TITLES)
}

fn picture_name(id: &TitleId, kind: ArtKind) -> String {
    let kind = match kind {
        ArtKind::Poster => "poster".to_string(),
        ArtKind::Background => "background".to_string(),
        ArtKind::Logo => "logo".to_string(),
        ArtKind::Thumbnail(e) => format!("s{}e{}", e.season, e.episode),
    };
    format!("{}/{id}.{kind}", shelf::PICTURES)
}

/// How long ago `at` was. A time in the future reads as long ago, so a skewed clock never keeps
/// a copy fresh.
fn age(at: SystemTime) -> Duration {
    SystemTime::now().duration_since(at).unwrap_or(Duration::MAX)
}

fn turned_off(why: &str) -> Error {
    Error::new(ErrorCode::Usage, format!("title lookups are off in config.yaml, and {why}"))
        .with_hint("turn title lookups on in config.yaml")
}

fn failed(what: &str, e: FetchError) -> Error {
    Error::new(ErrorCode::FetchFailed, format!("cannot {what}: {}", crate::fetch_reason(e, LOOKUP_WITHIN)))
        .with_hint("try again in a moment")
}

fn no_client(e: reqwest::Error) -> Error {
    Error::internal(format!("cannot start the HTTP client for the catalog: {e}"))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, kind: Kind, name: &str) -> TitleCard {
        TitleCard { id: id.parse().unwrap(), kind, name: name.into(), year: None, rating: None }
    }

    #[test]
    fn exact_names_come_first_and_each_group_takes_films_and_series_in_turn() {
        let films = vec![
            card("tt0000001", Kind::Film, "Dune: Part Two"),
            card("tt0000002", Kind::Film, "Dune"),
            card("tt0000003", Kind::Film, "Dune World"),
            card("tt0000004", Kind::Film, "DUNE"),
        ];
        let series = vec![card("tt0000011", Kind::Series, "Dune"), card("tt0000002", Kind::Film, "Dune")];
        let ids: Vec<String> = ranked(" dune", films, series).into_iter().map(|card| card.id.to_string()).collect();
        assert_eq!(ids, ["tt0000011", "tt0000002", "tt0000004", "tt0000001", "tt0000003"]);
    }

    #[test]
    fn a_picture_comes_from_https_or_the_one_plain_origin_allowed() {
        let local = Url::parse("http://127.0.0.1:8080/").unwrap().origin();
        let fetchable = |url: &str| may_fetch(&Url::parse(url).unwrap(), Some(&local));
        assert_eq!([fetchable("https://images.metahub.space/p"), fetchable("http://127.0.0.1:8080/p")], [true, true]);
        assert_eq!(
            [fetchable("http://127.0.0.1:8081/p"), fetchable("http://localhost:8080/p"), fetchable("ftp://x/p")],
            [false, false, false]
        );
        let catalog = || Catalog::new(&CatalogConfig { online: true, endpoint: None, cache_dir: None }).unwrap();
        let error = catalog().allow_plain_http_from("http://example.com:80").err().unwrap();
        assert_eq!(
            (error.code, error.message.as_str()),
            (ErrorCode::Usage, "http://example.com:80 is not a loopback HTTP address")
        );
        assert!(catalog().allow_plain_http_from("http://[::1]:9").is_ok());
    }
}
