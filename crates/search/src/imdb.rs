//! IMDb's suggestions: the titles its own search box lists for some words, best known first. They
//! need no key. The catalog searches with them, and subtitles find a file's IMDb ID with them.
//! Each reads IMDb's kinds of title its own way: the catalog lists films and series as cards, and
//! subtitles also take a video or a TV short as a film.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use tokio::time::Instant;
use torrential_core::title::{Title, normalized, year_slack};
use torrential_core::{Kind, TitleCard};
use url::Url;

use crate::catalog::Listed;
use crate::http::{FetchError, Http};

pub const BASE: &str = "https://v3.sg.media-imdb.com/";
/// The name of IMDb's endpoint among subtitles' endpoints.
pub const NAME: &str = "imdb";
const LARGEST: usize = 256 << 10;

/// The kinds of title the catalog lists, by IMDb's name. Anything else, such as a video, a game,
/// a single episode, or a person, it leaves out.
const LISTED: [(&str, Kind); 6] = [
    ("movie", Kind::Film),
    ("short", Kind::Film),
    ("tvMovie", Kind::Film),
    ("tvSpecial", Kind::Film),
    ("tvSeries", Kind::Series),
    ("tvMiniSeries", Kind::Series),
];

/// The kinds of title an episode's series can be, and the kinds a film can be, when subtitles look a
/// title up. Anything else, such as a single episode or a video game, is neither.
const SERIES: [&str; 2] = ["tvSeries", "tvMiniSeries"];
const FILMS: [&str; 6] = ["movie", "short", "tvMovie", "video", "tvSpecial", "tvShort"];

/// Wide enough for a card and a title page. IMDb's original poster is often 1000 or more.
const POSTER_WIDTH: u32 = 300;

#[derive(Deserialize)]
struct Suggestions {
    #[serde(default)]
    d: Vec<Suggestion>,
}

#[derive(Deserialize)]
pub struct Suggestion {
    /// A title's ID such as `tt1727587`, or a person's such as `nm0000001`.
    pub id: String,
    /// The title. People are listed too, by name.
    #[serde(default)]
    pub l: String,
    pub y: Option<u16>,
    /// The kind of title, such as `movie` or `tvSeries`. People have none.
    pub qid: Option<String>,
    /// A title's poster, or a person's photo.
    pub i: Option<Image>,
}

#[derive(Deserialize)]
pub struct Image {
    #[serde(rename = "imageUrl")]
    pub url: String,
}

/// What IMDb suggests for `words`, in its order.
pub async fn suggestions(
    http: &Http,
    base: &Url,
    words: &str,
    deadline: Instant,
) -> Result<Vec<Suggestion>, FetchError> {
    let path = format!("suggestion/x/{}.json", utf8_percent_encode(words, NON_ALPHANUMERIC));
    let url = base.join(&path).map_err(|e| FetchError::Connect(e.to_string()))?;
    let body = http.get(&url, LARGEST, deadline).await?;
    let suggestions: Suggestions = serde_json::from_slice(&body).map_err(|e| FetchError::Decode(e.to_string()))?;
    Ok(suggestions.d)
}

/// The films and series among `suggestions`, in IMDb's order, each with its poster about 300
/// pixels wide.
pub(crate) fn listed(suggestions: Vec<Suggestion>) -> Vec<Listed> {
    let listed = suggestions.into_iter().filter_map(|suggestion| {
        let (_, kind) = LISTED.iter().find(|(name, _)| suggestion.qid.as_deref() == Some(*name))?;
        let name = suggestion.l.trim();
        let card = TitleCard {
            id: suggestion.id.parse().ok()?,
            kind: *kind,
            name: (!name.is_empty()).then(|| name.to_string())?,
            year: suggestion.y,
            rating: None,
        };
        let poster = suggestion.i.and_then(|image| Url::parse(image.url.trim()).ok()).map(sized);
        Some(Listed { card, poster })
    });
    listed.collect()
}

/// `poster` about 300 pixels wide. IMDb's pictures are Amazon's, which resize to what a `._V1_SX300`
/// before the extension says, in place of whatever cropping or sizing the address had.
fn sized(mut poster: Url) -> Url {
    let path = poster.path();
    if let Some((stem, extension)) = path.rsplit_once('.').filter(|(_, extension)| !extension.contains('/')) {
        let stem = stem.find("._V1").map_or(stem, |at| &stem[..at]);
        let sized = format!("{stem}._V1_SX{POSTER_WIDTH}.{extension}");
        poster.set_path(&sized);
    }
    poster
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Matched {
    /// Such as `tt1727587`.
    pub id: String,
    /// The name and year IMDb has, with the episode the file's name gave.
    pub title: Title,
}

/// The first of `titles` that IMDb knows under that exact name. `None` when none is: subtitles for
/// a film that only resembles this one are worse than none.
pub async fn find(http: &Http, base: &Url, titles: &[Title], deadline: Instant) -> Result<Option<Matched>, FetchError> {
    for title in titles {
        let name = normalized(&title.name);
        if name.is_empty() {
            continue;
        }
        if let Some(matched) = pick(title, &suggestions(http, base, &name, deadline).await?) {
            return Ok(Some(matched));
        }
    }
    Ok(None)
}

/// The suggestion that is `title`: the same name, a film for a film and a series for an episode,
/// and the year the name carried. A film's year may be off by one, since a release can name the
/// year after its premiere. IMDb lists the best-known title first, which decides between equals.
fn pick(title: &Title, suggestions: &[Suggestion]) -> Option<Matched> {
    let name = normalized(&title.name);
    let kinds: &[&str] = if title.episode.is_some() { &SERIES } else { &FILMS };
    let fits = |s: &&Suggestion| {
        s.id.starts_with("tt") && normalized(&s.l) == name && s.qid.as_deref().is_some_and(|kind| kinds.contains(&kind))
    };
    let off_by = |s: &Suggestion| match (title.year, s.y) {
        (None, _) => Some(0),
        (Some(named), Some(listed)) => Some(named.abs_diff(listed)),
        (Some(_), None) => None,
    };
    let within = year_slack(title.episode.is_some());
    let candidates = suggestions.iter().filter(fits).filter_map(|s| Some((off_by(s)?, s)));
    let (_, best) = candidates.filter(|(off, _)| *off <= within).min_by_key(|(off, _)| *off)?;
    Some(Matched { id: best.id.clone(), title: Title { name: best.l.clone(), year: best.y, episode: title.episode } })
}

#[cfg(test)]
mod tests {
    use torrential_core::catalog::Episode;

    use super::*;

    #[test]
    fn a_poster_is_asked_for_300_pixels_wide() {
        let amazon = "https://m.media-amazon.com/images/M/";
        let cases = [
            ("MV5BMzc5NTUz@@._V1_.jpg", "MV5BMzc5NTUz@@._V1_SX300.jpg"),
            ("MV5BMDc1MDE0@._V1_CR276,54,1648,2473_.jpg", "MV5BMDc1MDE0@._V1_SX300.jpg"),
            ("MV5BMDc1MDE0@._V1_UX182_CR0,0,182,268_AL_.png", "MV5BMDc1MDE0@._V1_SX300.png"),
            ("MV5BNDA3OGZk.jpg", "MV5BNDA3OGZk._V1_SX300.jpg"),
        ];
        for (original, expected) in cases {
            let poster = sized(Url::parse(&format!("{amazon}{original}")).unwrap());
            assert_eq!(poster.as_str(), format!("{amazon}{expected}"));
        }
        let plain = "https://example.com/v1.2/poster";
        assert_eq!(sized(Url::parse(plain).unwrap()).as_str(), plain, "an address without an extension stays");
    }

    fn listed(id: &str, name: &str, year: Option<u16>, kind: Option<&str>) -> Suggestion {
        Suggestion { id: id.into(), l: name.into(), y: year, qid: kind.map(String::from), i: None }
    }

    fn film(name: &str, year: Option<u16>) -> Title {
        Title { name: name.into(), year, episode: None }
    }

    fn episode(name: &str, year: Option<u16>) -> Title {
        Title { name: name.into(), year, episode: Some(Episode { season: 1, episode: 2 }) }
    }

    #[test]
    fn only_the_same_title_of_the_same_kind_and_year_is_a_match() {
        let dune = [
            listed("nm0000001", "Dune", None, None),
            listed("tt15239678", "Dune: Part Two", Some(2024), Some("movie")),
            listed("tt1160419", "Dune", Some(2021), Some("movie")),
            listed("tt0087182", "Dune", Some(1984), Some("movie")),
            listed("tt0142032", "Dune", Some(2000), Some("tvMiniSeries")),
            listed("tt9999999", "Dune", Some(2021), Some("videoGame")),
        ];
        let id = |title: Title| pick(&title, &dune).map(|matched| matched.id);
        assert_eq!(id(film("Dune", Some(1984))).as_deref(), Some("tt0087182"));
        assert_eq!(id(film("dune", Some(2021))).as_deref(), Some("tt1160419"));
        assert_eq!(id(film("Dune", Some(2022))).as_deref(), Some("tt1160419"), "a release named a year late");
        assert_eq!(id(film("Dune", Some(1990))), None);
        assert_eq!(id(film("Dune", None)).as_deref(), Some("tt1160419"), "the best-known Dune");
        assert_eq!(id(film("Dune Part Two", Some(2024))).as_deref(), Some("tt15239678"));
        assert_eq!(id(film("Dune Part", Some(2024))), None, "a title that only resembles one");
        assert_eq!(id(episode("Dune", None)).as_deref(), Some("tt0142032"));
        assert_eq!(id(episode("Dune", Some(2000))).as_deref(), Some("tt0142032"));
        assert_eq!(id(episode("Dune", Some(2001))), None, "a series' year is the year it began");
    }

    #[test]
    fn a_match_carries_imdbs_name_and_the_files_episode() {
        let listing = [listed("tt0386676", "The Office", Some(2005), Some("tvSeries"))];
        let matched = pick(&episode("the office", None), &listing).unwrap();
        assert_eq!(matched.title, Title { name: "The Office".into(), year: Some(2005), ..episode("", None) });
    }
}
