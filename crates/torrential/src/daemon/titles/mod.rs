//! Films and series: finding one, playing it from the first source that works, and where a
//! person stopped.

pub mod attempt;
pub mod catalog_port;
pub mod shelf;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use torrential_core::watching::next;
use torrential_core::{
    Category, EpisodeInfo, Error, ErrorCode, Job, Kind, Preferences, Profile, Release, ReleaseId, SearchQuery, Skip,
    TitleDetails, TorrentId, VideoRef,
};

use catalog_port::{CatalogPort, label, season_episodes, wanted};
use shelf::Shelf;

/// How long a release that failed stays at the back of the line.
const FAILED_FOR: Duration = Duration::from_secs(6 * 60 * 60);
/// Ranked releases a play looks at. The rest rarely play better than these.
const MOST_CONSIDERED: usize = 12;

/// What the engine knows about films and series, beside its downloads.
pub struct Titles {
    pub catalog: Arc<dyn CatalogPort>,
    pub shelf: Shelf,
    pub timing: Timing,
    /// When each release last failed to play, and why. In memory, so a restart gives every one a
    /// chance.
    failed: Mutex<HashMap<ReleaseId, (Instant, Skip)>>,
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
    pub fn new(catalog: Arc<dyn CatalogPort>, shelf: Shelf, timing: Timing) -> Self {
        Self { catalog, shelf, timing, failed: Mutex::default() }
    }

    fn failed(&self, release: ReleaseId, why: Skip) {
        self.failed.lock().unwrap_or_else(PoisonError::into_inner).insert(release, (Instant::now(), why));
    }

    /// `video`'s sources in the order a play tries them and a list of sources shows them: the
    /// downloads in `jobs` that hold it, then the top of `ranked`. A download on this machine goes
    /// ahead of a release that is not, and a release that failed lately goes last, with why.
    /// Releases ranked below the top rarely play better, so neither shows them.
    pub fn line(&self, jobs: &[Job], video: &VideoRef, ranked: Vec<Release>) -> Vec<Release> {
        let here: BTreeSet<TorrentId> = jobs.iter().map(|job| job.id).collect();
        let failed = self.failed.lock().unwrap_or_else(PoisonError::into_inner);
        let failed_lately = |id: &ReleaseId| failed.get(id).filter(|(at, _)| at.elapsed() < FAILED_FOR);
        let mut seen = HashSet::new();
        let linked = jobs.iter().filter_map(|job| holding(job, video));
        let mut line: Vec<Release> = linked
            .chain(ranked.into_iter().take(MOST_CONSIDERED))
            .filter(|release| seen.insert(release.id))
            .map(|release| Release { failed: failed_lately(&release.id).map(|(_, why)| why.clone()), ..release })
            .collect();
        let is_here = |release: &Release| matches!(release.id, ReleaseId::Torrent(id) if here.contains(&id));
        line.sort_by_key(|release| (release.failed.is_some(), !is_here(release)));
        line
    }

    /// The film or episode `video` names, from the catalog's details alone. A series without an
    /// episode is its next episode to watch, or its first when the person has caught up.
    pub async fn resolve(&self, video: &VideoRef, now: i64) -> Result<Resolved, Error> {
        let details = self.catalog.title(&video.title).await?;
        let video = match (details.card.kind, video.episode) {
            (Kind::Series, None) => next_episode(&details, &self.shelf.watched(&video.title)?, now)?,
            _ => video.clone(),
        };
        season_episodes(&details, video.episode)?;
        let mut episodes = details.seasons.iter().flat_map(|season| &season.episodes);
        let episode = episodes.find(|info| Some(info.episode) == video.episode).cloned();
        if let Some(info) = &episode {
            aired(&details, info, now)?;
        }
        Ok(Resolved { video, details, episode })
    }

    /// The search for `title`'s releases, ranked for playing now with `preferences`. It asks the
    /// catalog which titles share the name, which takes the catalog's search. So a play asks for
    /// this only when it is about to search, and a title whose download is here plays without it.
    pub async fn query(&self, title: &Resolved, preferences: Preferences) -> Result<SearchQuery, Error> {
        let namesakes = self.catalog.namesakes(&title.details.card).await?;
        let wanted = wanted(&title.details, &namesakes, title.video.episode)?;
        let text = wanted.queries().into_iter().next().unwrap_or_else(|| wanted.name.clone());
        Ok(SearchQuery {
            category: Some(Category::Video),
            profile: Profile { preferences, ..Profile::default() },
            wanted: Some(wanted),
            ..SearchQuery::new(text)
        })
    }
}

/// A film or an episode a person asked for, as the catalog describes it.
pub struct Resolved {
    pub video: VideoRef,
    pub details: TitleDetails,
    pub episode: Option<EpisodeInfo>,
}

impl Resolved {
    /// The film's name and year, or the series' name and the episode, as a person reads it.
    pub fn name(&self) -> String {
        crate::output::views::title_name(&self.details.card, self.video.episode)
    }
}

/// A download on this machine that a title play made for `video`'s title and that holds it, as a
/// release to try.
fn holding(job: &Job, video: &VideoRef) -> Option<Release> {
    let link = job.title.as_ref().filter(|link| link.title == video.title && link.span.covers(video.episode))?;
    Some(Release {
        id: ReleaseId::Torrent(job.id),
        name: job.name.clone().unwrap_or_else(|| job.id.to_hex()),
        size: job.progress.size,
        seeders: None,
        leechers: None,
        published: None,
        category: Some(Category::Video),
        flags: BTreeSet::new(),
        sources: Vec::new(),
        magnet: None,
        torrent_url: None,
        info_url: None,
        span: Some(link.span),
        swarm: None,
        failed: None,
    })
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
    use torrential_core::title::Span;
    use torrential_core::{InfoHash, JobStatus, Progress, Selection, TitleLink, Watched};

    fn video(text: &str) -> VideoRef {
        text.parse().unwrap()
    }

    async fn resolve(titles: &Titles, text: &str) -> Result<String, (ErrorCode, String)> {
        titles.resolve(&video(text), 1_762_990_000).await.map(|r| r.name()).map_err(|e| (e.code, e.message))
    }

    fn titles(dir: &tempfile::TempDir) -> Titles {
        let path = dir.path().join("state.db");
        drop(crate::daemon::store::Store::open(&path).unwrap());
        Titles::new(Arc::new(catalog_port::Fake::default()), Shelf::open(&path).unwrap(), Timing::default())
    }

    #[test]
    fn sources_go_downloads_here_first_then_the_top_twelve_and_those_that_failed_lately_last_with_why() {
        let dir = tempfile::tempdir().unwrap();
        let titles = titles(&dir);
        let release = |n: u8| Release {
            id: ReleaseId::Torrent(TorrentId::from_bytes([n; 20])),
            name: format!("release {n}"),
            size: None,
            seeders: None,
            leechers: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            sources: Vec::new(),
            magnet: None,
            torrent_url: None,
            info_url: None,
            span: None,
            swarm: None,
            failed: None,
        };
        let film = TitleLink { title: "tt0063350".parse().unwrap(), span: Span::Film, release: None };
        let job = |n: u8, title: Option<TitleLink>| Job {
            id: TorrentId::from_bytes([n; 20]),
            info_hash: InfoHash::V1([n; 20]),
            name: Some(format!("release {n}")),
            dir: "/downloads".into(),
            path: None,
            status: JobStatus::Queued,
            progress: Progress::default(),
            selection: Selection::All,
            added_at: 0,
            title,
        };
        titles.failed(release(2).id, Skip::NoVideo);
        titles.failed(release(40).id, Skip::NoData { waited_s: 30 });
        let jobs = [job(3, None), job(40, Some(film))];
        let line = |jobs: &[Job], video: &str, ranked: &[u8]| -> Vec<(u8, Option<Skip>)> {
            let ranked = ranked.iter().map(|n| release(*n)).collect();
            let line = titles.line(jobs, &video.parse().unwrap(), ranked).into_iter();
            line.map(|r| (r.name.trim_start_matches("release ").parse().unwrap(), r.failed)).collect()
        };
        let fine = |n: u8| (n, None);
        assert_eq!(line(&[], "tt0063350", &[1, 2, 3]), [fine(1), fine(3), (2, Some(Skip::NoVideo))]);
        assert_eq!(
            line(&jobs, "tt0063350", &[1, 2, 3, 4]),
            [fine(3), fine(1), fine(4), (40, Some(Skip::NoData { waited_s: 30 })), (2, Some(Skip::NoVideo))],
            "a download here leads, and one that failed lately goes last even when it holds the film"
        );
        assert_eq!(line(&jobs, "tt0100258", &[1]), [fine(1)], "a download made for another title holds nothing");
        let many: Vec<u8> = (1..=20).collect();
        let top: Vec<(u8, Option<Skip>)> =
            [1].into_iter().chain(3..=12).map(fine).chain([(2, Some(Skip::NoVideo))]).collect();
        assert_eq!(line(&[], "tt0063350", &many), top);
    }

    #[tokio::test]
    async fn a_series_plays_its_next_aired_episode_and_never_one_that_has_not_aired() {
        let dir = tempfile::tempdir().unwrap();
        let titles = titles(&dir);
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
