//! Cinemeta's answers, read into the core catalog types. Nothing else sees its JSON.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;
use serde_json::Value;
use torrential_core::catalog::Country;
use torrential_core::title::Episode;
use torrential_core::{Art, ArtKind, EpisodeInfo, Kind, Season, TitleCard, TitleDetails, TitleId};
use url::Url;

use crate::dates;

/// The path segment Cinemeta names a kind by.
pub fn segment(kind: Kind) -> &'static str {
    match kind {
        Kind::Film => "movie",
        Kind::Series => "series",
    }
}

fn kind(segment: &str) -> Option<Kind> {
    [Kind::Film, Kind::Series].into_iter().find(|kind| self::segment(*kind) == segment)
}

/// One title of a search answer, with the poster address the search named.
pub struct Listed {
    pub card: TitleCard,
    pub poster: Option<Url>,
}

/// A title's details, with the address of each picture Cinemeta named for it.
pub struct Described {
    pub details: TitleDetails,
    pub pictures: Pictures,
}

#[derive(Default)]
pub struct Pictures {
    poster: Option<Url>,
    background: Option<Url>,
    logo: Option<Url>,
    thumbnails: HashMap<Episode, Url>,
}

impl Pictures {
    pub fn address(&self, kind: ArtKind) -> Option<&Url> {
        match kind {
            ArtKind::Poster => self.poster.as_ref(),
            ArtKind::Background => self.background.as_ref(),
            ArtKind::Logo => self.logo.as_ref(),
            ArtKind::Thumbnail(episode) => self.thumbnails.get(&episode),
        }
    }
}

/// A string or a number, as Cinemeta writes years and ratings either way.
#[derive(Deserialize)]
#[serde(untagged)]
enum Loose {
    Text(String),
    Number(serde_json::Number),
}

impl Loose {
    fn text(&self) -> String {
        match self {
            Loose::Text(text) => text.clone(),
            Loose::Number(number) => number.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Listing {
    metas: Option<Vec<Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    id: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    release_info: Option<Loose>,
    year: Option<Loose>,
    imdb_rating: Option<Loose>,
    description: Option<String>,
    genres: Option<Vec<String>>,
    runtime: Option<Loose>,
    cast: Option<Vec<String>>,
    country: Option<String>,
    poster: Option<String>,
    background: Option<String>,
    logo: Option<String>,
    videos: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct Answer {
    meta: Option<Meta>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Video {
    season: Option<u32>,
    episode: Option<u32>,
    number: Option<u32>,
    name: Option<String>,
    title: Option<String>,
    released: Option<String>,
    first_aired: Option<String>,
    overview: Option<String>,
    description: Option<String>,
    thumbnail: Option<String>,
}

impl Meta {
    /// The card for this title, when its ID is a title ID and it has a name.
    fn card(&self, kind: Kind) -> Option<TitleCard> {
        let name = self.name.as_deref().map(unescaped).filter(|name| !name.is_empty())?;
        let year = self.release_info.as_ref().or(self.year.as_ref()).and_then(|year| first_year(&year.text()));
        Some(TitleCard {
            id: self.id.parse().ok()?,
            kind,
            name,
            year,
            rating: self.imdb_rating.as_ref().and_then(|rating| rating_of(&rating.text())),
        })
    }
}

/// The titles of a search answer in Cinemeta's order. A title without a title ID or a name is left out.
pub fn listing(body: &[u8], kind: Kind) -> Result<Vec<Listed>, serde_json::Error> {
    let listing: Listing = serde_json::from_slice(body)?;
    let metas = listing.metas.unwrap_or_default().into_iter().filter_map(|meta| Meta::deserialize(meta).ok());
    let listed = metas.filter_map(|meta| Some(Listed { card: meta.card(kind)?, poster: address(meta.poster) }));
    Ok(listed.collect())
}

/// The title `id` as a details answer describes it. `None` when the answer names no such title, as
/// Cinemeta's `{}` for a film asked for as a series.
pub fn described(body: &[u8], id: &TitleId) -> Result<Option<Described>, serde_json::Error> {
    let Some(meta) = serde_json::from_slice::<Answer>(body)?.meta else { return Ok(None) };
    let Some(kind) = meta.kind.as_deref().and_then(kind) else { return Ok(None) };
    let Some(card) = meta.card(kind).filter(|card| card.id == *id) else { return Ok(None) };
    let videos = meta.videos.unwrap_or_default().into_iter().filter_map(|video| Video::deserialize(video).ok());
    let episodes: Vec<(EpisodeInfo, Option<Url>)> = match kind {
        Kind::Film => Vec::new(),
        Kind::Series => videos.filter_map(episode).collect(),
    };
    let pictures = Pictures {
        poster: address(meta.poster),
        background: address(meta.background),
        logo: address(meta.logo),
        thumbnails: episodes.iter().filter_map(|(info, url)| Some((info.episode, url.clone()?))).collect(),
    };
    let details = TitleDetails {
        card,
        synopsis: meta.description.as_deref().map(unescaped).filter(|text| !text.is_empty()),
        genres: meta.genres.unwrap_or_default().iter().map(|genre| unescaped(genre)).collect(),
        runtime_min: meta.runtime.and_then(|runtime| minutes(&runtime.text())),
        cast: meta.cast.unwrap_or_default().iter().map(|name| unescaped(name)).collect(),
        countries: meta.country.as_deref().map(Country::listed_in).unwrap_or_default(),
        art: Art {
            poster: pictures.poster.is_some(),
            background: pictures.background.is_some(),
            logo: pictures.logo.is_some(),
        },
        seasons: seasons(episodes.into_iter().map(|(info, _)| info)),
    };
    Ok(Some(Described { details, pictures }))
}

fn episode(video: Video) -> Option<(EpisodeInfo, Option<Url>)> {
    let episode = Episode { season: video.season?, episode: video.episode.or(video.number)? };
    let text = |text: Option<String>| text.as_deref().map(unescaped).filter(|text| !text.is_empty());
    let thumbnail = address(video.thumbnail);
    let info = EpisodeInfo {
        episode,
        name: text(video.name.or(video.title)),
        aired: video.released.or(video.first_aired).as_deref().and_then(dates::rfc3339),
        overview: text(video.overview.or(video.description)),
        thumbnail: thumbnail.is_some(),
    };
    Some((info, thumbnail))
}

/// Seasons by number with specials last, and each season's episodes by number. The first of
/// two videos with the same season and number stays.
fn seasons(episodes: impl Iterator<Item = EpisodeInfo>) -> Vec<Season> {
    let mut by_season: BTreeMap<(bool, u32), BTreeMap<u32, EpisodeInfo>> = BTreeMap::new();
    for info in episodes {
        let Episode { season, episode } = info.episode;
        by_season.entry((season == 0, season)).or_default().entry(episode).or_insert(info);
    }
    let seasons = by_season
        .into_iter()
        .map(|((_, number), episodes)| Season { number, episodes: episodes.into_values().collect() });
    seasons.collect()
}

fn address(text: Option<String>) -> Option<Url> {
    Url::parse(text?.trim()).ok()
}

/// The year a `releaseInfo` such as `1968` or `2008–2013` starts with.
fn first_year(text: &str) -> Option<u16> {
    let digits: String = text.trim().chars().take_while(char::is_ascii_digit).collect();
    if digits.len() == 4 { digits.parse().ok() } else { None }
}

/// The minutes of a `runtime` such as `96 min`.
fn minutes(text: &str) -> Option<u32> {
    let text = text.trim();
    let digits = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let unit = text[digits..].trim_start();
    if unit.is_empty() || unit.starts_with("min") { text[..digits].parse().ok().filter(|&n| n > 0) } else { None }
}

fn rating_of(text: &str) -> Option<f32> {
    text.trim().parse().ok().filter(|rating| (0.0..=10.0).contains(rating))
}

/// `text` with its HTML character references decoded, as Cinemeta leaves them in names such as `O&apos;Dea`.
fn unescaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest[1..].find(';').filter(|&end| end <= 8).and_then(|end| {
            let reference = &rest[1..=end];
            let char = match reference {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let code = reference.strip_prefix('#')?;
                    let number = match code.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16),
                        None => code.parse(),
                    };
                    char::from_u32(number.ok()?)?
                }
            };
            Some((char, end + 2))
        });
        match decoded {
            Some((char, used)) => {
                out.push(char);
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_references_in_names_are_decoded() {
        assert_eq!(unescaped("Judith O&apos;Dea"), "Judith O'Dea");
        assert_eq!(unescaped("Tom &amp; Jerry &#8211; &#x41;mok &lt;3"), "Tom & Jerry – Amok <3");
        assert_eq!(unescaped("Fast & Furious; R&D &unknown; &"), "Fast & Furious; R&D &unknown; &");
    }

    #[test]
    fn years_runtimes_and_ratings_read_from_cinemetas_text() {
        assert_eq!(
            [first_year("1968"), first_year("2008–2013"), first_year("2016–"), first_year("")],
            [Some(1968), Some(2008), Some(2016), None]
        );
        assert_eq!(
            [minutes("96 min"), minutes("49min"), minutes("14"), minutes("1h 36min"), minutes("")],
            [Some(96), Some(49), Some(14), None, None]
        );
        assert_eq!([rating_of("7.8"), rating_of(""), rating_of("N/A"), rating_of("11")], [Some(7.8), None, None, None]);
    }

    #[test]
    fn a_series_seasons_run_by_number_with_specials_last() {
        let body = br#"{"meta":{"id":"tt0903747","type":"series","name":"Show","videos":[
            {"season":2,"number":1,"name":"B"},
            {"season":0,"episode":1,"name":"Special"},
            {"season":1,"episode":2,"name":"A2"},
            {"season":1,"episode":1,"name":"A1"},
            {"season":1,"episode":1,"name":"A1 again"},
            {"season":null,"episode":3},
            "not a video"]}}"#;
        let described = described(body, &"tt0903747".parse().unwrap()).unwrap().unwrap();
        let named =
            |e: &EpisodeInfo| format!("{}x{} {}", e.episode.season, e.episode.episode, e.name.as_deref().unwrap());
        let seasons: Vec<Vec<String>> =
            described.details.seasons.iter().map(|s| s.episodes.iter().map(named).collect()).collect();
        assert_eq!(seasons, [vec!["1x1 A1", "1x2 A2"], vec!["2x1 B"], vec!["0x1 Special"]]);
    }

    #[test]
    fn an_answer_about_another_title_or_no_title_describes_nothing() {
        let id: TitleId = "tt0063350".parse().unwrap();
        let name = |body: &str| described(body.as_bytes(), &id).unwrap().map(|d| d.details.card.name);
        let film = |id: &str, kind: &str| format!(r#"{{"meta":{{"id":"{id}","type":"{kind}","name":"Film"}}}}"#);
        assert_eq!(name(&film("tt0063350", "movie")).as_deref(), Some("Film"));
        assert_eq!(name(&film("tt0100258", "movie")), None);
        assert_eq!(name(&film("tt0063350", "channel")), None);
        assert_eq!(name("{}"), None);
        assert!(described(b"[]", &id).is_err());
    }
}
