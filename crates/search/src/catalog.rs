//! Films and series from Cinemeta, the catalog Stremio uses, and from IMDb's suggestions, read
//! into the core catalog types. A search asks both, since IMDb finds what Cinemeta's search
//! misses, such as short films. Details come from Cinemeta.
//!
//! Details and pictures are kept under the cache folder, so a title page opens from disk and a
//! picture is fetched once. Picture addresses come only from Cinemeta's and IMDb's answers and
//! never leave this module: callers name a picture by title ID and kind.

mod cinemeta;
pub(crate) mod imdb;
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

use self::cinemeta::Described;
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
/// The hosts the catalogs' pictures are on: Cinemeta's under metahub.space, IMDb's under
/// media-amazon.com, and image.tmdb.org, where Cinemeta's episode pictures redirect. A host
/// matches an entry that it is or ends with after a dot. Checked live on 2026-11-13.
const PICTURE_HOSTS: &[&str] = &["metahub.space", "media-amazon.com", "image.tmdb.org"];
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
    /// Where IMDb's suggestions are asked for. `None` is https://v3.sg.media-imdb.com.
    pub imdb_endpoint: Option<String>,
    /// Where details and pictures are kept. `None` keeps them in memory.
    pub cache_dir: Option<PathBuf>,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self { online: true, endpoint: None, imdb_endpoint: None, cache_dir: None }
    }
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
    Elsewhere,
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
            Lack::Elsewhere => format!("the {what} for {id} is on a host pictures are not fetched from"),
            Lack::NotImage => format!("the {what} for {id} is not a JPEG, PNG, or WebP picture"),
            Lack::TooLarge => format!("the {what} for {id} is larger than {} MiB", LARGEST_PICTURE >> 20),
        };
        Error::new(ErrorCode::NotFound, message)
    }
}

/// One title of a search answer, with the poster address the search named.
struct Listed {
    card: TitleCard,
    poster: Option<Url>,
}

/// Cinemeta and IMDb's suggestions, behind the core catalog types.
pub struct Catalog {
    http: Http,
    /// Follows only redirects to an address a picture may come from.
    pictures: Http,
    picture_from: PictureFrom,
    cinemeta: Url,
    imdb: Url,
    online: bool,
    shelf: Shelf,
    /// The kind and poster address of each title recent searches listed.
    listed: Mutex<HashMap<TitleId, (Kind, Option<Url>)>>,
}

/// Where a picture may be fetched from. Each redirect is checked again.
#[derive(Clone)]
struct PictureFrom {
    /// The host of a Cinemeta mirror the person set in config.yaml.
    mirror: Option<String>,
    /// A plain HTTP loopback origin, for tests against a local server.
    plain: Option<Origin>,
}

impl PictureFrom {
    /// An `https` address on port 443 with no user name, whose host is a name under
    /// [`PICTURE_HOSTS`] or the mirror's, or an address on the plain origin.
    fn allows(&self, url: &Url) -> bool {
        if self.plain.as_ref().is_some_and(|origin| url.origin() == *origin) {
            return true;
        }
        let Some(Host::Domain(host)) = url.host() else { return false };
        let known = PICTURE_HOSTS
            .iter()
            .any(|known| host == *known || host.strip_suffix(known).is_some_and(|head| head.ends_with('.')));
        let trusted = known || self.mirror.as_deref() == Some(host);
        trusted
            && url.scheme() == "https"
            && url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
    }

    /// A client for pictures. A redirect to an address a picture may not come from stops, and
    /// its answer then reads as a picture elsewhere.
    fn client(&self) -> Result<Http, Error> {
        let from = self.clone();
        let policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= REDIRECTS {
                attempt.error("too many redirects")
            } else if from.allows(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        });
        Http::redirecting(AGENT, false, policy).map_err(no_client)
    }
}

impl Catalog {
    /// Fails with `usage` when an endpoint is not a URL.
    pub fn new(config: &CatalogConfig) -> Result<Catalog, Error> {
        let base = |url: &str, name: &str| {
            let slashed = if url.ends_with('/') { url.to_string() } else { format!("{url}/") };
            Url::parse(&slashed)
                .map_err(|e| Error::new(ErrorCode::Usage, format!("the {name} endpoint {url} is not a URL: {e}")))
        };
        let cinemeta = base(config.endpoint.as_deref().unwrap_or(BASE), "catalog")?;
        let mirror = match cinemeta.host() {
            Some(Host::Domain(host)) if config.endpoint.is_some() => Some(host.to_string()),
            _ => None,
        };
        let picture_from = PictureFrom { mirror, plain: None };
        Ok(Catalog {
            http: Http::named(AGENT, false).map_err(no_client)?,
            pictures: picture_from.client()?,
            picture_from,
            cinemeta,
            imdb: base(config.imdb_endpoint.as_deref().unwrap_or(imdb::BASE), "IMDb")?,
            online: config.online,
            shelf: Shelf::new(config.cache_dir.clone()),
            listed: Mutex::default(),
        })
    }

    /// Lets pictures also come over plain HTTP from `origin`, which must be a loopback address
    /// such as a test's local server.
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
        self.picture_from.plain = Some(url.origin());
        self.pictures = self.picture_from.client()?;
        Ok(self)
    }

    /// False when title lookups are off, and the catalog answers only from what it keeps.
    pub fn online(&self) -> bool {
        self.online
    }

    /// Films and series named like `words`: the films and series IMDb suggests, in IMDb's order,
    /// then those Cinemeta lists that IMDb did not. Of Cinemeta's, titles whose normalized name is
    /// the words come first, then the rest, each group in Cinemeta's order with films and series
    /// in turn. Answers when IMDb or either of Cinemeta's listings does. Empty when turned off.
    pub async fn search(&self, words: &str) -> Result<Vec<TitleCard>, Error> {
        if !self.online || words.trim().is_empty() {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + LOOKUP_WITHIN;
        let (suggested, films, series) = tokio::join!(
            self.suggested(words, deadline),
            self.listing(Kind::Film, words, deadline),
            self.listing(Kind::Series, words, deadline)
        );
        if let (Err(e), Err(_), Err(_)) = (&suggested, &films, &series) {
            return Err(failed("search IMDb or Cinemeta", e.clone()));
        }
        let listed = ranked(words, films.unwrap_or_default(), series.unwrap_or_default());
        Ok(self.remember(merged(suggested.unwrap_or_default(), listed)))
    }

    /// The titles of `card`'s kind under its normalized name, `card` among them, in the order a
    /// search lists them, IMDb's first. Whoever plays `card` tells from this whether another title
    /// shares its name and which one is listed first. Empty when turned off.
    pub async fn namesakes(&self, card: &TitleCard) -> Result<Vec<TitleCard>, Error> {
        if !self.online {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + LOOKUP_WITHIN;
        let (suggested, listed) =
            tokio::join!(self.suggested(&card.name, deadline), self.listing(card.kind, &card.name, deadline));
        if let (Err(e), Err(_)) = (&suggested, &listed) {
            return Err(failed("search IMDb or Cinemeta", e.clone()));
        }
        let name = normalized(&card.name);
        let titles = self.remember(merged(suggested.unwrap_or_default(), listed.unwrap_or_default()));
        Ok(titles.into_iter().filter(|other| other.kind == card.kind && normalized(&other.name) == name).collect())
    }

    /// From disk while fresh, a film for 30 days and a series for 1 day. A stale copy answers
    /// when Cinemeta does not. Turned off, only a kept copy answers.
    pub async fn title(&self, id: &TitleId) -> Result<TitleDetails, Error> {
        Ok(self.described(id).await?.details)
    }

    /// The copy of a title's details kept on this machine, however old, without asking Cinemeta.
    pub async fn kept_title(&self, id: &TitleId) -> Option<TitleDetails> {
        Some(self.kept(id).await?.1.details)
    }

    /// A picture Cinemeta named for this title, fetched once and kept. Only from an `https`
    /// address on a host the catalogs keep pictures on, at most 5 MiB, and only bytes that start
    /// like JPEG, PNG, or WebP.
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
            Some(url) if !self.picture_from.allows(&url) => Lack::Elsewhere,
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
                Err(FetchError::Status(status)) if (300..400).contains(&status) => Lack::Elsewhere,
                Err(FetchError::Status(status)) if (400..500).contains(&status) => Lack::Status(status),
                Err(e) => return Err(failed(&format!("fetch a picture for {id}"), e)),
            },
        };
        let error = lack.error(id, kind);
        self.shelf.put(&missed, error.message.as_bytes()).await;
        Err(error)
    }

    /// IMDb's films and series for `words`, in its order.
    async fn suggested(&self, words: &str, deadline: Instant) -> Result<Vec<Listed>, FetchError> {
        Ok(imdb::listed(imdb::suggestions(&self.http, &self.imdb, &asked(words), deadline).await?))
    }

    /// Cinemeta's titles of `kind` for `words`.
    async fn listing(&self, kind: Kind, words: &str, deadline: Instant) -> Result<Vec<Listed>, FetchError> {
        let words = asked(words);
        let query = utf8_percent_encode(&words, NON_ALPHANUMERIC);
        let path = format!("catalog/{}/top/search={query}.json", cinemeta::segment(kind));
        let url = self.cinemeta.join(&path).map_err(|e| FetchError::Connect(e.to_string()))?;
        let body = self.http.get(&url, LARGEST_LISTING, deadline).await?;
        cinemeta::listing(&body, kind).map_err(|e| FetchError::Decode(e.to_string()))
    }

    /// The cards of `listed`, remembering each title's kind and poster address so its details and
    /// poster need no second search.
    fn remember(&self, listed: Vec<Listed>) -> Vec<TitleCard> {
        let mut remembered = lock(&self.listed);
        if remembered.len() + listed.len() > LISTED_MOST {
            remembered.clear();
        }
        let cards = listed.into_iter().map(|Listed { card, poster }| {
            remembered.insert(card.id.clone(), (card.kind, poster));
            card
        });
        cards.collect()
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
            let url = self.cinemeta.join(&path).map_err(|e| FetchError::Connect(e.to_string()))?;
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

/// `words` as they are asked for: lowercase with single spaces, so the same words make the same
/// request.
fn asked(words: &str) -> String {
    words.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Titles whose normalized name is `words` first, then the rest. Each group keeps Cinemeta's
/// order, taking films and series in turn. A title listed twice stays where it first appears.
fn ranked(words: &str, films: Vec<Listed>, series: Vec<Listed>) -> Vec<Listed> {
    let wanted = normalized(words);
    let (mut films, mut series) = (films.into_iter(), series.into_iter());
    let turns = std::iter::from_fn(|| match (films.next(), series.next()) {
        (None, None) => None,
        (film, show) => Some(film.into_iter().chain(show)),
    });
    let mut seen = HashSet::new();
    let mut ranked: Vec<Listed> = turns.flatten().filter(|listed| seen.insert(listed.card.id.clone())).collect();
    ranked.sort_by_key(|listed| normalized(&listed.card.name) != wanted);
    ranked
}

/// IMDb's titles in IMDb's order, then Cinemeta's others in Cinemeta's. A title both list keeps
/// IMDb's place and card, and Cinemeta's poster when IMDb names none.
fn merged(imdb: Vec<Listed>, mut cinemeta: Vec<Listed>) -> Vec<Listed> {
    let mut merged = Vec::with_capacity(imdb.len() + cinemeta.len());
    for mut listed in imdb {
        if let Some(at) = cinemeta.iter().position(|other| other.card.id == listed.card.id) {
            listed.poster = listed.poster.or(cinemeta.remove(at).poster);
        }
        merged.push(listed);
    }
    merged.extend(cinemeta);
    let mut seen = HashSet::new();
    merged.retain(|listed| seen.insert(listed.card.id.clone()));
    merged
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
    Error::new(ErrorCode::FetchFailed, format!("cannot {what}: {}", crate::fetch_reason(e)))
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

    fn listed(id: &str, kind: Kind, name: &str) -> Listed {
        Listed {
            card: TitleCard { id: id.parse().unwrap(), kind, name: name.into(), year: None, rating: None },
            poster: None,
        }
    }

    fn ids(listed: Vec<Listed>) -> Vec<String> {
        listed.into_iter().map(|listed| listed.card.id.to_string()).collect()
    }

    #[test]
    fn exact_names_come_first_and_each_group_takes_films_and_series_in_turn() {
        let films = vec![
            listed("tt0000001", Kind::Film, "Dune: Part Two"),
            listed("tt0000002", Kind::Film, "Dune"),
            listed("tt0000003", Kind::Film, "Dune World"),
            listed("tt0000004", Kind::Film, "DUNE"),
        ];
        let series = vec![listed("tt0000011", Kind::Series, "Dune"), listed("tt0000002", Kind::Film, "Dune")];
        assert_eq!(
            ids(ranked(" dune", films, series)),
            ["tt0000011", "tt0000002", "tt0000004", "tt0000001", "tt0000003"]
        );
    }

    #[test]
    fn imdbs_titles_lead_and_a_title_both_list_appears_once_with_imdbs_poster_first() {
        let poster = |listed: Listed, url: &str| Listed { poster: Some(Url::parse(url).unwrap()), ..listed };
        let imdb = vec![
            poster(listed("tt0000002", Kind::Film, "Dune"), "https://imdb/2"),
            listed("tt0000003", Kind::Film, "Dune World"),
            listed("tt0000002", Kind::Film, "Dune"),
        ];
        let cinemeta = vec![
            listed("tt0000001", Kind::Film, "Dune"),
            poster(listed("tt0000003", Kind::Film, "Dune World"), "https://cinemeta/3"),
            poster(listed("tt0000002", Kind::Film, "Dune"), "https://cinemeta/2"),
        ];
        let merged = merged(imdb, cinemeta);
        let posters: Vec<Option<&str>> = merged.iter().map(|listed| listed.poster.as_ref().map(Url::as_str)).collect();
        assert_eq!(posters, [Some("https://imdb/2"), Some("https://cinemeta/3"), None]);
        assert_eq!(ids(merged), ["tt0000002", "tt0000003", "tt0000001"]);
    }

    #[test]
    fn a_picture_comes_only_from_a_picture_host_the_mirror_or_the_one_plain_origin_allowed() {
        let config = CatalogConfig { endpoint: Some("https://cinemeta.mirror.example".into()), ..Default::default() };
        let catalog = Catalog::new(&config).unwrap().allow_plain_http_from("http://127.0.0.1:8080").unwrap();
        let fetchable = |url: &str| catalog.picture_from.allows(&Url::parse(url).unwrap());
        let allowed = [
            "https://images.metahub.space/poster/small/tt0063350/img",
            "https://episodes.metahub.space/tt0903747/1/1/w780.jpg",
            "https://image.tmdb.org/t/p/w780/x.jpg",
            "https://m.media-amazon.com/images/M/x.jpg",
            "https://images.metahub.space:443/p",
            "https://cinemeta.mirror.example/poster.jpg",
            "http://127.0.0.1:8080/p",
        ];
        assert_eq!(allowed.map(fetchable), [true; 7]);
        let refused = [
            "http://images.metahub.space/p",
            "https://images.metahub.space:8443/p",
            "https://someone@images.metahub.space/p",
            "https://someone:secret@m.media-amazon.com/p",
            "https://evilmetahub.space/p",
            "https://images.metahub.space.example.com/p",
            "https://api.tmdb.org/p",
            "https://example.com/p",
            "https://localhost/p",
            "https://127.0.0.1/p",
            "https://[::1]/p",
            "https://10.0.0.1/p",
            "http://cinemeta.mirror.example/p",
            "http://127.0.0.1:8081/p",
            "http://localhost:8080/p",
            "ftp://images.metahub.space/p",
        ];
        assert_eq!(refused.map(fetchable), [false; 16]);
        let error =
            Catalog::new(&CatalogConfig::default()).unwrap().allow_plain_http_from("http://example.com:80").err();
        let error = error.unwrap();
        assert_eq!(
            (error.code, error.message.as_str()),
            (ErrorCode::Usage, "http://example.com:80 is not a loopback HTTP address")
        );
        let unset = Catalog::new(&CatalogConfig::default()).unwrap();
        assert!(!unset.picture_from.allows(&Url::parse("https://v3-cinemeta.strem.io/p").unwrap()));
        assert!(unset.allow_plain_http_from("http://[::1]:9").is_ok());
    }
}
