//! Where a person stopped each film and episode, what of a title to play next, and the steps a
//! title play reports.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::catalog::{EpisodeInfo, Kind, TitleCard, TitleDetails, TitleId, VideoRef};
use crate::error::Error;
use crate::id::{ReleaseId, TorrentId};
use crate::release::{Release, SourceReport};
use crate::title::{Episode, Span};

/// Where a person stopped one film or episode. Whether it is finished follows from the position.
#[derive(Clone, PartialEq, Debug)]
pub struct Watched {
    pub video: VideoRef,
    /// Seconds.
    pub position: f64,
    /// Seconds. `None` until the player knows it.
    pub duration: Option<f64>,
    /// The release it played from.
    pub release: Option<ReleaseId>,
    /// Unix seconds of the last update.
    pub at: i64,
}

impl Watched {
    /// The last 20 seconds, or the last 3% when that is longer, are credits, so stopping there
    /// finishes the video.
    pub fn finished(&self) -> bool {
        self.duration.is_some_and(|duration| duration > 0.0 && duration - self.position <= (duration * 0.03).max(20.0))
    }
}

impl Serialize for Watched {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a> {
            video: &'a VideoRef,
            position: f64,
            duration: Option<f64>,
            finished: bool,
            at: i64,
        }
        let Watched { video, position, duration, release: _, at } = self;
        Fields { video, position: *position, duration: *duration, finished: self.finished(), at: *at }
            .serialize(serializer)
    }
}

/// Reads what `Serialize` writes, which leaves out the release, so a read has none.
impl<'de> Deserialize<'de> for Watched {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Fields {
            video: VideoRef,
            position: f64,
            duration: Option<f64>,
            at: i64,
        }
        let Fields { video, position, duration, at } = Fields::deserialize(deserializer)?;
        Ok(Watched { video, position, duration, release: None, at })
    }
}

/// What Play on a title page plays.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Next {
    pub video: VideoRef,
    /// Seconds to resume at, or `None` to start from the beginning.
    pub resume_at: Option<f64>,
}

/// What of a title to play: an unfinished film or episode to resume, or else the first aired
/// episode after the one finished last that is not finished too. `None` when the film is
/// finished or the series is caught up. Specials, in season 0, never come next. An episode the
/// catalog does not list, or that has not aired, counts as never watched.
pub fn next(details: &TitleDetails, watched: &[Watched], now: i64) -> Option<Next> {
    let title = &details.card.id;
    let resume = |w: &Watched| Next { video: w.video.clone(), resume_at: Some(w.position) };
    let at = |episode: Option<Episode>| VideoRef { title: title.clone(), episode };
    let entry = |video: &VideoRef| watched.iter().find(|w| w.video == *video);
    if details.card.kind == Kind::Film {
        let film = at(None);
        return match entry(&film) {
            Some(w) if w.finished() => None,
            Some(w) => Some(resume(w)),
            None => Some(Next { video: film, resume_at: None }),
        };
    }
    let mut aired: Vec<Episode> = details
        .seasons
        .iter()
        .flat_map(|season| &season.episodes)
        .filter(|e| e.aired.is_some_and(|aired| aired <= now))
        .map(|e| e.episode)
        .collect();
    aired.sort();
    let listed =
        |w: &&Watched| w.video.title == *title && w.video.episode.is_some_and(|e| aired.binary_search(&e).is_ok());
    let latest = watched.iter().filter(listed).max_by_key(|w| w.at);
    if let Some(w) = latest.filter(|w| !w.finished()) {
        return Some(resume(w));
    }
    aired.retain(|e| e.season > 0);
    let last = latest.and_then(|w| w.video.episode);
    let from = last.and_then(|last| aired.iter().position(|e| *e == last)).map_or(0, |i| i + 1);
    aired[from..].iter().map(|e| at(Some(*e))).find_map(|video| match entry(&video) {
        Some(w) if w.finished() => None,
        Some(w) => Some(resume(w)),
        None => Some(Next { video, resume_at: None }),
    })
}

/// The title a download was made for, and what of it the download holds.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TitleLink {
    pub title: TitleId,
    pub span: Span,
    /// The search result the download came from.
    pub release: Option<ReleaseId>,
}

/// One step of a title play. The web app reads these as NDJSON lines, and the last is `playing`
/// or `failed`.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AttemptEvent {
    Resolved {
        video: VideoRef,
        title: TitleCard,
        episode: Option<EpisodeInfo>,
    },
    Searched {
        found: u32,
        sources: Vec<SourceReport>,
    },
    Trying {
        rank: u32,
        of: u32,
        release: Release,
    },
    Check {
        check: Check,
    },
    Skipped {
        release: ReleaseId,
        reason: Skip,
    },
    /// The download and the file that play. The HTTP route adds how to play it.
    Playing {
        video: VideoRef,
        release: Release,
        job: TorrentId,
        file: u32,
        resume_at: Option<f64>,
    },
    Failed {
        error: Error,
        tried: Vec<Tried>,
    },
}

/// What a play waits for from the source under test.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    FileList,
    Start,
    FirstBytes,
}

/// Why a play moved past a source.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum Skip {
    NoFileList {
        waited_s: u32,
    },
    /// No file reads as this film or episode, or the largest is a sample.
    NoVideo,
    /// The video plays in a browser, ffmpeg is missing, and the file is not MP4, M4V, MOV, or WebM.
    Unplayable {
        container: String,
    },
    TooLarge {
        error: Error,
    },
    NoData {
        waited_s: u32,
    },
    Stalled,
    Failed {
        error: Error,
    },
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Tried {
    pub release: ReleaseId,
    pub reason: Skip,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::catalog::{Art, Season};
    use crate::error::ErrorCode;
    use crate::release::{Category, SourceOutcome};

    const HASH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

    fn video(text: &str) -> VideoRef {
        text.parse().unwrap()
    }

    fn watched(text: &str, position: f64, duration: Option<f64>, at: i64) -> Watched {
        Watched { video: video(text), position, duration, release: None, at }
    }

    #[test]
    fn the_credits_count_as_finished() {
        let read = [
            watched("tt0063350", 3460.0, Some(3487.0), 0),
            watched("tt0063350", 1290.4, Some(2820.0), 0),
            watched("tt0063350", 590.0, Some(600.0), 0),
            watched("tt0063350", 570.0, Some(600.0), 0),
            watched("tt0063350", 0.0, None, 0),
            watched("tt0063350", 5.0, Some(0.0), 0),
        ]
        .map(|w| w.finished());
        assert_eq!(read, [true, false, true, false, false, false]);
    }

    #[test]
    fn watched_writes_whether_it_is_finished_and_not_the_release() {
        let entry = Watched {
            release: Some(ReleaseId::Torrent(HASH.parse().unwrap())),
            ..watched("tt0903747:3:5", 1290.4, Some(2820.0), 1762990000)
        };
        assert_eq!(
            serde_json::to_string(&entry).unwrap(),
            r#"{"video":"tt0903747:3:5","position":1290.4,"duration":2820.0,"finished":false,"at":1762990000}"#
        );
        let unknown = watched("tt0063350", 12.0, None, 1762990000);
        assert_eq!(
            serde_json::to_string(&unknown).unwrap(),
            r#"{"video":"tt0063350","position":12.0,"duration":null,"finished":false,"at":1762990000}"#
        );
    }

    fn card(id: &str, kind: Kind, name: &str, year: u16) -> TitleCard {
        TitleCard { id: id.parse().unwrap(), kind, name: name.into(), year: Some(year), rating: None }
    }

    fn details(card: TitleCard, seasons: Vec<Season>) -> TitleDetails {
        TitleDetails {
            card,
            synopsis: None,
            genres: Vec::new(),
            runtime_min: None,
            cast: Vec::new(),
            countries: Default::default(),
            art: Art::default(),
            seasons,
        }
    }

    fn season(number: u32, aired: &[Option<i64>]) -> Season {
        let episodes = (1..)
            .zip(aired)
            .map(|(episode, &aired)| EpisodeInfo {
                episode: Episode { season: number, episode },
                name: None,
                aired,
                overview: None,
                thumbnail: false,
            })
            .collect();
        Season { number, episodes }
    }

    const NOW: i64 = 1_762_990_000;

    #[test]
    fn a_film_resumes_until_it_is_finished() {
        let film = details(card("tt0063350", Kind::Film, "Night of the Living Dead", 1968), Vec::new());
        let next = |watched: &[Watched]| next(&film, watched, NOW);
        assert_eq!(next(&[]), Some(Next { video: video("tt0063350"), resume_at: None }));
        let halfway = [watched("tt0063350", 1834.2, Some(5760.0), 10), watched("tt0100258", 10.0, None, 20)];
        assert_eq!(next(&halfway), Some(Next { video: video("tt0063350"), resume_at: Some(1834.2) }));
        assert_eq!(next(&[watched("tt0063350", 5750.0, Some(5760.0), 10)]), None);
    }

    /// Breaking Bad with one special, two episodes in season 1, and in season 2 one aired, one
    /// to air tomorrow, and one with no date.
    fn breaking_bad() -> TitleDetails {
        let aired = Some(NOW - 86_400);
        details(
            card("tt0903747", Kind::Series, "Breaking Bad", 2008),
            vec![season(0, &[aired]), season(1, &[aired, aired]), season(2, &[aired, Some(NOW + 86_400), None])],
        )
    }

    #[test]
    fn a_series_goes_on_from_the_episode_finished_last() {
        let series = breaking_bad();
        let next = |watched: &[Watched]| next(&series, watched, NOW).map(|n| (n.video.to_string(), n.resume_at));
        let done = |text: &str, at: i64| watched(text, 2810.0, Some(2820.0), at);
        assert_eq!(next(&[]), Some(("tt0903747:1:1".into(), None)));
        assert_eq!(next(&[done("tt0903747:1:1", 10)]), Some(("tt0903747:1:2".into(), None)));
        assert_eq!(
            next(&[done("tt0903747:1:1", 10), watched("tt0903747:1:2", 600.0, Some(2820.0), 20)]),
            Some(("tt0903747:1:2".into(), Some(600.0)))
        );
        assert_eq!(next(&[done("tt0903747:1:1", 10), done("tt0903747:1:2", 20)]), Some(("tt0903747:2:1".into(), None)));
        assert_eq!(next(&[done("tt0903747:2:1", 30)]), None, "2x2 has not aired");
        assert_eq!(
            next(&[done("tt0903747:1:1", 10), done("tt0903747:0:1", 40)]),
            Some(("tt0903747:1:2".into(), None)),
            "after a special, from the start"
        );
        assert_eq!(
            next(&[done("tt0903747:1:2", 50), watched("tt0903747:1:1", 300.0, Some(2820.0), 5)]),
            Some(("tt0903747:2:1".into(), None)),
            "an older unfinished episode before the last one stays behind"
        );
        assert_eq!(next(&[watched("tt0063350", 10.0, None, 99)]), Some(("tt0903747:1:1".into(), None)));
    }

    #[test]
    fn an_episode_the_catalog_does_not_list_or_that_has_not_aired_is_never_resumed() {
        let series = breaking_bad();
        let next = |watched: &[Watched]| next(&series, watched, NOW).map(|n| (n.video.to_string(), n.resume_at));
        let done = watched("tt0903747:1:1", 2810.0, Some(2820.0), 10);
        let started = ["tt0903747:2:2", "tt0903747:2:3", "tt0903747:9:1"];
        let read = started.map(|video| next(&[done.clone(), watched(video, 300.0, Some(2820.0), 20)]));
        assert_eq!(
            read,
            [
                Some(("tt0903747:1:2".into(), None)),
                Some(("tt0903747:1:2".into(), None)),
                Some(("tt0903747:1:2".into(), None))
            ]
        );
    }

    fn release() -> Release {
        Release {
            id: ReleaseId::Torrent(HASH.parse().unwrap()),
            name: "Night.of.the.Living.Dead.1968.1080p.BluRay.x264".into(),
            size: Some(1_500_000_000),
            seeders: Some(40),
            leechers: None,
            published: None,
            category: Some(Category::Video),
            flags: BTreeSet::new(),
            sources: vec!["knaben".into()],
            magnet: None,
            torrent_url: None,
            info_url: None,
            span: Some(Span::Film),
            swarm: None,
            failed: None,
        }
    }

    const RELEASE: &str = concat!(
        r#"{"id":"9b521219bc48550140a666f74a29048ed78e933b","name":"Night.of.the.Living.Dead.1968.1080p.BluRay.x264","#,
        r#""size":1500000000,"seeders":40,"leechers":null,"published":null,"category":"video","#,
        r#""quality":{"resolution":"1080p","codec":"h264","hdr":false},"flags":[],"sources":["knaben"],"#,
        r#""magnet":null,"torrent_url":null,"info_url":null,"span":"film"}"#
    );

    #[test]
    fn each_step_of_a_play_writes_the_json_the_web_app_reads() {
        let error = |code, message: &str| Error::new(code, message);
        let steps = [
            AttemptEvent::Resolved {
                video: video("tt0063350"),
                title: TitleCard {
                    rating: Some(7.8),
                    ..card("tt0063350", Kind::Film, "Night of the Living Dead", 1968)
                },
                episode: None,
            },
            AttemptEvent::Searched {
                found: 5,
                sources: vec![SourceReport {
                    source: "archive.org".into(),
                    outcome: SourceOutcome::Ok { results: 2 },
                    elapsed_ms: 812,
                }],
            },
            AttemptEvent::Trying { rank: 1, of: 5, release: release() },
            AttemptEvent::Check { check: Check::FileList },
            AttemptEvent::Skipped {
                release: ReleaseId::Torrent(HASH.parse().unwrap()),
                reason: Skip::NoFileList { waited_s: 20 },
            },
            AttemptEvent::Playing {
                video: video("tt0903747:3:5"),
                release: release(),
                job: HASH.parse().unwrap(),
                file: 4,
                resume_at: Some(1834.2),
            },
            AttemptEvent::Failed {
                error: error(ErrorCode::NoSource, "No source of Night of the Living Dead (1968) plays now"),
                tried: vec![Tried {
                    release: ReleaseId::Torrent(HASH.parse().unwrap()),
                    reason: Skip::NoData { waited_s: 30 },
                }],
            },
        ];
        let written: Vec<String> = steps.iter().map(|step| serde_json::to_string(step).unwrap()).collect();
        assert_eq!(
            written,
            [
                r#"{"type":"resolved","video":"tt0063350","title":{"id":"tt0063350","kind":"film","name":"Night of the Living Dead","year":1968,"rating":7.8},"episode":null}"#.to_string(),
                r#"{"type":"searched","found":5,"sources":[{"source":"archive.org","outcome":"ok","results":2,"elapsed_ms":812}]}"#.to_string(),
                format!(r#"{{"type":"trying","rank":1,"of":5,"release":{RELEASE}}}"#),
                r#"{"type":"check","check":"file_list"}"#.to_string(),
                format!(r#"{{"type":"skipped","release":"{HASH}","reason":{{"reason":"no_file_list","waited_s":20}}}}"#),
                format!(r#"{{"type":"playing","video":"tt0903747:3:5","release":{RELEASE},"job":"{HASH}","file":4,"resume_at":1834.2}}"#),
                format!(r#"{{"type":"failed","error":{{"code":"no_source","message":"No source of Night of the Living Dead (1968) plays now"}},"tried":[{{"release":"{HASH}","reason":{{"reason":"no_data","waited_s":30}}}}]}}"#),
            ]
        );
        let read: Vec<AttemptEvent> = written.iter().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(read, steps);

        let checks = [Check::FileList, Check::Start, Check::FirstBytes].map(|c| serde_json::to_string(&c).unwrap());
        assert_eq!(checks, [r#""file_list""#, r#""start""#, r#""first_bytes""#]);

        let full = error(ErrorCode::DiskFull, "not enough space");
        let skips = [
            Skip::NoFileList { waited_s: 20 },
            Skip::NoVideo,
            Skip::Unplayable { container: "avi".into() },
            Skip::TooLarge { error: full.clone() },
            Skip::NoData { waited_s: 30 },
            Skip::Stalled,
            Skip::Failed { error: error(ErrorCode::StorageError, "cannot write") },
        ]
        .map(|skip| serde_json::to_string(&skip).unwrap());
        assert_eq!(
            skips,
            [
                r#"{"reason":"no_file_list","waited_s":20}"#,
                r#"{"reason":"no_video"}"#,
                r#"{"reason":"unplayable","container":"avi"}"#,
                r#"{"reason":"too_large","error":{"code":"disk_full","message":"not enough space"}}"#,
                r#"{"reason":"no_data","waited_s":30}"#,
                r#"{"reason":"stalled"}"#,
                r#"{"reason":"failed","error":{"code":"storage_error","message":"cannot write"}}"#,
            ]
        );
    }

    #[test]
    fn a_title_link_writes_its_span_and_the_release_it_came_from() {
        let link = TitleLink {
            title: "tt0903747".parse().unwrap(),
            span: Span::Season(1),
            release: Some(ReleaseId::Torrent(HASH.parse().unwrap())),
        };
        let json = serde_json::to_string(&link).unwrap();
        assert_eq!(json, format!(r#"{{"title":"tt0903747","span":"s1","release":"{HASH}"}}"#));
        assert_eq!(serde_json::from_str::<TitleLink>(&json).unwrap(), link);
        let unlisted = TitleLink { release: None, ..link };
        assert_eq!(serde_json::to_string(&unlisted).unwrap(), r#"{"title":"tt0903747","span":"s1","release":null}"#);
    }
}
