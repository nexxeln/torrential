//! Finds a film's or a series' IMDb ID from its title, with the suggestions IMDb's own search box
//! uses.

use tokio::time::Instant;
use torrential_core::title::{Title, normalized};
use url::Url;

pub use crate::catalog::imdb::BASE;
use crate::catalog::imdb::{Suggestion, suggestions};
use crate::http::{FetchError, Http};

pub const NAME: &str = "imdb";

/// The kinds of title an episode's series can be, and the kinds a film can be. Anything else,
/// such as a single episode or a video game, is neither.
const SERIES: [&str; 2] = ["tvSeries", "tvMiniSeries"];
const FILMS: [&str; 6] = ["movie", "short", "tvMovie", "video", "tvSpecial", "tvShort"];

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
    let within = if title.episode.is_some() { 0 } else { 1 };
    let candidates = suggestions.iter().filter(fits).filter_map(|s| Some((off_by(s)?, s)));
    let (_, best) = candidates.filter(|(off, _)| *off <= within).min_by_key(|(off, _)| *off)?;
    Some(Matched { id: best.id.clone(), title: Title { name: best.l.clone(), year: best.y, episode: title.episode } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::title::Episode;

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
