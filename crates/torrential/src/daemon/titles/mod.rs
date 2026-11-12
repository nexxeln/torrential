//! Films and series: finding one, playing it from the first source that works, and where a
//! person stopped.

pub mod attempt;
pub mod catalog_port;
pub mod shelf;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use torrential_core::title::Wanted;
use torrential_core::watching::next;
use torrential_core::{
    Category, EpisodeInfo, Error, ErrorCode, Kind, Preferences, Profile, ReleaseId, SearchQuery, TitleDetails, VideoRef,
};

use catalog_port::{CatalogPort, label};
use shelf::Shelf;

/// How long a release that failed stays at the back of the line.
const FAILED_FOR: Duration = Duration::from_secs(6 * 60 * 60);

/// What the engine knows about films and series, beside its downloads.
pub struct Titles {
    pub catalog: Arc<dyn CatalogPort>,
    /// False when config.yaml turns title lookups off. The catalog then answers only from what
    /// it keeps on this machine.
    pub online: bool,
    pub shelf: Shelf,
    pub timing: Timing,
    /// When each release last failed to play. In memory, so a restart gives every one a chance.
    failed: Mutex<HashMap<ReleaseId, Instant>>,
}

/// Every deadline of a title play. Tests shrink them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timing {
    /// For a source's file list.
    pub file_list: Duration,
    /// For the first bytes of the video, from the start of its download.
    pub first_bytes: Duration,
    /// For the whole play.
    pub budget: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            file_list: Duration::from_secs(20),
            first_bytes: Duration::from_secs(30),
            budget: Duration::from_secs(180),
        }
    }
}

impl Titles {
    pub fn new(catalog: Arc<dyn CatalogPort>, online: bool, shelf: Shelf, timing: Timing) -> Self {
        Self { catalog, online, shelf, timing, failed: Mutex::default() }
    }

    fn failed(&self, release: ReleaseId) {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner).insert(release, Instant::now());
    }

    fn failed_lately(&self, release: &ReleaseId) -> bool {
        let failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        failed.get(release).is_some_and(|at| at.elapsed() < FAILED_FOR)
    }

    /// The film or episode `video` names, with how to find its releases. A series without an
    /// episode is its next episode to watch, or its first when the person has caught up.
    pub async fn resolve(&self, video: &VideoRef, now: i64) -> Result<Resolved, Error> {
        let video = match video.episode {
            Some(_) => video.clone(),
            None => {
                let details = self.catalog.title(&video.title).await?;
                match details.card.kind {
                    Kind::Film => video.clone(),
                    Kind::Series => next_episode(&details, &self.shelf.watched(&video.title)?, now)?,
                }
            }
        };
        let (details, wanted) = self.catalog.wanted(&video).await?;
        let episode = match video.episode {
            None => None,
            Some(e) => {
                let info = details.seasons.iter().flat_map(|s| &s.episodes).find(|info| info.episode == e);
                let info = info.cloned().ok_or_else(|| {
                    Error::new(ErrorCode::NotFound, format!("{} is not an episode of {}", label(e), details.card.name))
                })?;
                aired(&details, &info, now)?;
                Some(info)
            }
        };
        Ok(Resolved { video, details, wanted, episode })
    }
}

/// A film or an episode a person asked for, as the catalog describes it.
pub struct Resolved {
    pub video: VideoRef,
    pub details: TitleDetails,
    pub wanted: Wanted,
    pub episode: Option<EpisodeInfo>,
}

impl Resolved {
    /// The film's name and year, or the series' name and the episode, as a person reads it.
    pub fn name(&self) -> String {
        let card = &self.details.card;
        match (self.video.episode, card.year) {
            (Some(e), _) => format!("{} {}", card.name, label(e)),
            (None, Some(year)) => format!("{} ({year})", card.name),
            (None, None) => card.name.clone(),
        }
    }

    /// The search for this title's releases, ranked for playing now with `preferences`.
    pub fn query(&self, preferences: Preferences) -> SearchQuery {
        let text = self.wanted.queries().into_iter().next().unwrap_or_else(|| self.wanted.name.clone());
        SearchQuery {
            category: Some(Category::Video),
            profile: Profile { preferences, ..Profile::default() },
            wanted: Some(self.wanted.clone()),
            ..SearchQuery::new(text)
        }
    }
}

/// The episode a series plays next, or its first aired one when the person has caught up.
fn next_episode(details: &TitleDetails, watched: &[torrential_core::Watched], now: i64) -> Result<VideoRef, Error> {
    if let Some(next) = next(details, watched, now) {
        return Ok(next.video);
    }
    let first = details
        .seasons
        .iter()
        .filter(|season| season.number > 0)
        .flat_map(|season| &season.episodes)
        .filter(|e| e.aired.is_some_and(|aired| aired <= now))
        .map(|e| e.episode)
        .min();
    let first = first
        .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("no episode of {} has aired yet", details.card.name)))?;
    Ok(VideoRef { title: details.card.id.clone(), episode: Some(first) })
}

fn aired(details: &TitleDetails, info: &EpisodeInfo, now: i64) -> Result<(), Error> {
    let episode = format!("{} {}", details.card.name, label(info.episode));
    match info.aired {
        Some(aired) if aired <= now => Ok(()),
        Some(aired) => {
            let day = &crate::output::format::timestamp(aired)[..10];
            Err(Error::new(ErrorCode::Usage, format!("{episode} airs on {day}, so it has no sources yet")))
        }
        None => Err(Error::new(ErrorCode::Usage, format!("{episode} has no air date yet, so it has no sources"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::Watched;

    fn video(text: &str) -> VideoRef {
        text.parse().unwrap()
    }

    async fn resolve(titles: &Titles, text: &str) -> Result<String, (ErrorCode, String)> {
        titles.resolve(&video(text), 1_762_990_000).await.map(|r| r.name()).map_err(|e| (e.code, e.message))
    }

    #[tokio::test]
    async fn a_series_plays_its_next_aired_episode_and_never_one_that_has_not_aired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        drop(crate::daemon::store::Store::open(&path).unwrap());
        let catalog = Arc::new(catalog_port::Fake::default());
        let titles = Titles::new(catalog, true, Shelf::open(&path).unwrap(), Timing::default());
        assert_eq!(resolve(&titles, "tt9999901").await, Ok("Harbor Lights S01E01".into()));
        assert_eq!(resolve(&titles, "tt0063350").await, Ok("Night of the Living Dead (1968)".into()));
        let finished = |text: &str, at| Watched {
            video: video(text),
            position: 2630.0,
            duration: Some(2640.0),
            release: None,
            at,
        };
        titles.shelf.watch(&finished("tt9999901:1:1", 10)).unwrap();
        assert_eq!(resolve(&titles, "tt9999901").await, Ok("Harbor Lights S01E02".into()));
        titles.shelf.watch(&finished("tt9999901:2:1", 20)).unwrap();
        assert_eq!(
            resolve(&titles, "tt9999901").await,
            Ok("Harbor Lights S01E01".into()),
            "caught up, so from the start"
        );
        assert_eq!(
            resolve(&titles, "tt9999901:2:2").await,
            Err((ErrorCode::Usage, "Harbor Lights S02E02 airs on 2100-01-01, so it has no sources yet".into()))
        );
        assert_eq!(
            resolve(&titles, "tt9999901:2:3").await,
            Err((ErrorCode::Usage, "Harbor Lights S02E03 has no air date yet, so it has no sources".into()))
        );
        assert_eq!(
            resolve(&titles, "tt9999901:4:1").await,
            Err((ErrorCode::NotFound, "S04E01 is not an episode of Harbor Lights".into()))
        );
    }
}
