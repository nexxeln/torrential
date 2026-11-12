//! Films and series as a catalog describes them, and the IDs that name them everywhere else.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

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
    /// IMDb's rating, 0 to 10.
    pub rating: Option<f32>,
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
    pub art: Art,
    /// Empty for a film. Season 0 holds specials.
    pub seasons: Vec<Season>,
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
}
