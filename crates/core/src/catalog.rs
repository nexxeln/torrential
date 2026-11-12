//! Films and series as a catalog describes them, and the IDs that name them everywhere else.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize, Serializer};

use crate::error::{Error, ErrorCode};
use crate::title::Episode;

/// An IMDb title ID such as `tt0063350`: `tt` and 7 to 10 digits.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TitleId(String);

impl TitleId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TitleId {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let digits = text.strip_prefix("tt").unwrap_or_default();
        if (7..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) {
            Ok(TitleId(text.to_string()))
        } else {
            Err(Error::new(ErrorCode::InvalidRef, format!("{text:?} is not a title ID"))
                .with_hint("a title ID looks like tt0063350"))
        }
    }
}

impl TryFrom<String> for TitleId {
    type Error = Error;

    fn try_from(text: String) -> Result<Self, Error> {
        text.parse()
    }
}

impl From<TitleId> for String {
    fn from(id: TitleId) -> String {
        id.0
    }
}

impl fmt::Display for TitleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Film,
    Series,
}

/// What plays: a film, or one episode of a series. A series ID alone names the series, and
/// whoever plays it chooses the episode. The text form is `tt0063350` or `tt0903747:3:5`.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VideoRef {
    pub title: TitleId,
    pub episode: Option<Episode>,
}

impl FromStr for VideoRef {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let invalid = || {
            Error::new(ErrorCode::InvalidRef, format!("{text:?} names no film or episode"))
                .with_hint("use a title ID such as tt0063350, or tt0903747:3:5 for season 3 episode 5")
        };
        let mut parts = text.split(':');
        let title: TitleId = parts.next().unwrap_or_default().parse()?;
        let episode = match (parts.next(), parts.next(), parts.next()) {
            (None, ..) => None,
            (Some(season), Some(episode), None) => Some(Episode {
                season: season.parse().map_err(|_| invalid())?,
                episode: episode.parse().map_err(|_| invalid())?,
            }),
            _ => return Err(invalid()),
        };
        Ok(VideoRef { title, episode })
    }
}

impl TryFrom<String> for VideoRef {
    type Error = Error;

    fn try_from(text: String) -> Result<Self, Error> {
        text.parse()
    }
}

impl From<VideoRef> for String {
    fn from(video: VideoRef) -> String {
        video.to_string()
    }
}

impl fmt::Display for VideoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.episode {
            Some(Episode { season, episode }) => write!(f, "{}:{season}:{episode}", self.title),
            None => write!(f, "{}", self.title),
        }
    }
}

/// Enough of a title to list it.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TitleCard {
    pub id: TitleId,
    pub kind: Kind,
    pub name: String,
    /// A film's year, or the year a series began.
    pub year: Option<u16>,
    /// IMDb's rating, 0 to 10, written with the one decimal IMDb shows.
    #[serde(serialize_with = "one_decimal")]
    pub rating: Option<f32>,
}

/// Writes 7.6 as `7.6`, which an `f32` would write as `7.599999904632568`.
fn one_decimal<S: Serializer>(rating: &Option<f32>, s: S) -> Result<S::Ok, S::Error> {
    rating.map(|rating| (f64::from(rating) * 10.0).round() / 10.0).serialize(s)
}

/// One title as the catalog describes it. Pictures are served by title ID, so only whether each
/// exists shows here.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct TitleDetails {
    #[serde(flatten)]
    pub card: TitleCard,
    pub synopsis: Option<String>,
    pub genres: Vec<String>,
    pub runtime_min: Option<u32>,
    pub cast: Vec<String>,
    /// The countries the title is from, of those whose codes release names carry. Empty when it
    /// is from none of them or the catalog does not say.
    #[serde(default)]
    pub countries: Vec<Country>,
    pub art: Art,
    /// Empty for a film. Season 0 holds specials.
    pub seasons: Vec<Season>,
}

/// A country whose code release names carry to tell same-named series apart, as `US` does in
/// `The.Office.US.S02E01`. The text form is that code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Country {
    Us,
    Uk,
    Au,
    Ca,
    Nz,
    Ie,
}

impl Country {
    const ALL: [Country; 6] = [Country::Us, Country::Uk, Country::Au, Country::Ca, Country::Nz, Country::Ie];

    /// The code release names use, and the names a catalog writes the country by.
    fn spellings(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Country::Us => ("US", &["United States", "United States of America", "USA"]),
            Country::Uk => ("UK", &["United Kingdom", "UK"]),
            Country::Au => ("AU", &["Australia"]),
            Country::Ca => ("CA", &["Canada"]),
            Country::Nz => ("NZ", &["New Zealand"]),
            Country::Ie => ("IE", &["Ireland"]),
        }
    }

    pub fn code(self) -> &'static str {
        self.spellings().0
    }

    /// The country a code such as `US` or `uk` stands for.
    pub fn of_code(code: &str) -> Option<Country> {
        Country::ALL.into_iter().find(|country| country.code().eq_ignore_ascii_case(code))
    }

    /// The countries a catalog lists as `United States, United Kingdom`, in its order. A country
    /// without a code here is left out.
    pub fn listed_in(text: &str) -> Vec<Country> {
        let mut countries = Vec::new();
        for name in text.split(',').map(str::trim) {
            let named = |country: &Country| country.spellings().1.iter().any(|n| n.eq_ignore_ascii_case(name));
            if let Some(country) = Country::ALL.into_iter().find(named)
                && !countries.contains(&country)
            {
                countries.push(country);
            }
        }
        countries
    }
}

impl TryFrom<String> for Country {
    type Error = String;

    fn try_from(code: String) -> Result<Self, String> {
        Country::of_code(&code).ok_or_else(|| format!("{code:?} is not a country code"))
    }
}

impl From<Country> for String {
    fn from(country: Country) -> String {
        country.code().to_string()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Art {
    pub poster: bool,
    pub background: bool,
    pub logo: bool,
}

/// One picture of a title.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ArtKind {
    Poster,
    Background,
    Logo,
    Thumbnail(Episode),
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Season {
    pub number: u32,
    pub episodes: Vec<EpisodeInfo>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct EpisodeInfo {
    #[serde(flatten)]
    pub episode: Episode,
    pub name: Option<String>,
    /// Unix seconds. `None` when no date is announced.
    pub aired: Option<i64>,
    pub overview: Option<String>,
    pub thumbnail: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_video_reads_from_its_text_and_writes_back() {
        let film: VideoRef = "tt0063350".parse().unwrap();
        assert_eq!((film.title.as_str(), film.episode), ("tt0063350", None));
        let episode: VideoRef = "tt0903747:3:5".parse().unwrap();
        assert_eq!(episode.episode, Some(Episode { season: 3, episode: 5 }));
        assert_eq!(episode.to_string(), "tt0903747:3:5");
        assert_eq!(serde_json::to_string(&episode).unwrap(), "\"tt0903747:3:5\"");
        assert_eq!("tt33296064".parse::<TitleId>().unwrap().as_str(), "tt33296064");
        for bad in ["", "tt12", "0063350", "tt006335x", "tt0903747:3", "tt0903747:3:5:1", "tt0903747:a:b"] {
            assert_eq!(bad.parse::<VideoRef>().unwrap_err().code, ErrorCode::InvalidRef, "{bad}");
        }
    }

    #[test]
    fn a_country_reads_from_its_code_or_the_names_a_catalog_lists() {
        assert_eq!(
            ["US", "uk", "Ie", "GB", "USA"].map(Country::of_code),
            [Some(Country::Us), Some(Country::Uk), Some(Country::Ie), None, None]
        );
        assert_eq!(Country::listed_in("United States"), [Country::Us]);
        assert_eq!(
            Country::listed_in("Ireland, United Kingdom, Netherlands, United States"),
            [Country::Ie, Country::Uk, Country::Us]
        );
        assert_eq!(
            Country::listed_in("Australia,New Zealand, Canada, USA, United States"),
            [Country::Au, Country::Nz, Country::Ca, Country::Us]
        );
        assert_eq!(Country::listed_in("Netherlands"), []);
        assert_eq!(serde_json::to_string(&[Country::Uk, Country::Nz]).unwrap(), r#"["UK","NZ"]"#);
        assert_eq!(serde_json::from_str::<Vec<Country>>(r#"["US","ca"]"#).unwrap(), [Country::Us, Country::Ca]);
        assert!(serde_json::from_str::<Country>(r#""FR""#).is_err());
    }

    #[test]
    fn a_rating_is_written_with_one_decimal() {
        let card = TitleCard {
            id: "tt1727587".parse().unwrap(),
            kind: Kind::Film,
            name: "Sintel".into(),
            year: Some(2010),
            rating: Some(7.6),
        };
        let json = serde_json::to_string(&card).unwrap();
        assert_eq!(json, r#"{"id":"tt1727587","kind":"film","name":"Sintel","year":2010,"rating":7.6}"#);
        assert_eq!(serde_json::from_str::<TitleCard>(&json).unwrap(), card);
        let unrated = serde_json::to_string(&TitleCard { rating: None, ..card }).unwrap();
        assert_eq!(unrated, r#"{"id":"tt1727587","kind":"film","name":"Sintel","year":2010,"rating":null}"#);
    }
}
