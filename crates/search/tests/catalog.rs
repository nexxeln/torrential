//! The catalog against a local server that answers with what Cinemeta answered when the fixtures
//! were recorded. Picture addresses in those answers point at the same server.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrential_core::catalog::{Countries, Country};
use torrential_core::title::Episode;
use torrential_core::{ArtKind, ErrorCode, Kind, TitleCard, TitleId};
use torrential_search::catalog::{Catalog, CatalogConfig, ImageFormat};

const NOTLD: &str = "tt0063350";
const SINTEL: &str = "tt1727587";
const BREAKING_BAD: &str = "tt0903747";
const SINTEL_POSTER: &str = "/images/poster/small/tt1727587/img";

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cinemeta").join(name)).unwrap()
}

fn id(text: &str) -> TitleId {
    text.parse().unwrap()
}

/// One answer: a status, a body, and where a redirect goes.
#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
}

fn ok(body: Vec<u8>) -> Reply {
    Reply { status: 200, body, location: None }
}

fn status(status: u16) -> Reply {
    Reply { status, body: b"no".to_vec(), location: None }
}

/// Serves each path that starts with a route's prefix, and remembers every path asked for.
struct Server {
    base: String,
    asked: Arc<Mutex<Vec<String>>>,
    agents: Arc<Mutex<Vec<String>>>,
}

impl Server {
    /// `routes` gets the server's own base URL, for answers that point back at it.
    async fn start(routes: impl FnOnce(&str) -> Vec<(&'static str, Reply)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = routes(&base);
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let agents: Arc<Mutex<Vec<String>>> = Arc::default();
        let (seen, named) = (asked.clone(), agents.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                seen.lock().unwrap().push(path.clone());
                let agent = head
                    .lines()
                    .find_map(|line| line.to_ascii_lowercase().strip_prefix("user-agent: ").map(String::from));
                named.lock().unwrap().push(agent.unwrap_or_default());
                let reply = routes.iter().find(|(prefix, _)| path.starts_with(prefix)).map(|(_, reply)| reply.clone());
                let reply = reply.unwrap_or_else(|| status(404));
                let location = reply.location.map(|to| format!("Location: {to}\r\n")).unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 {} X\r\nConnection: close\r\n{location}Content-Length: {}\r\n\r\n",
                    reply.status,
                    reply.body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&reply.body).await;
                let _ = socket.shutdown().await;
            }
        });
        Server { base, asked, agents }
    }

    /// What Cinemeta answered, with each picture address moved to this server.
    async fn recorded() -> Server {
        Server::start(recorded).await
    }

    fn catalog(&self, online: bool, cache_dir: Option<&Path>) -> Catalog {
        let config =
            CatalogConfig { online, endpoint: Some(self.base.clone()), cache_dir: cache_dir.map(Path::to_path_buf) };
        Catalog::new(&config).unwrap().allow_plain_http_from(&self.base).unwrap()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// A recorded answer with its picture addresses moved to `base`.
fn answer(base: &str, name: &str) -> Reply {
    let text = String::from_utf8(fixture(name)).unwrap();
    let text = text
        .replace("https://images.metahub.space", &format!("{base}/images"))
        .replace("https://episodes.metahub.space", &format!("{base}/episodes"));
    ok(text.into_bytes())
}

fn recorded(base: &str) -> Vec<(&'static str, Reply)> {
    vec![
        (
            "/catalog/movie/top/search=night%20of%20the%20living%20dead.json",
            answer(base, "search-movie-night-of-the-living-dead.json"),
        ),
        (
            "/catalog/series/top/search=night%20of%20the%20living%20dead.json",
            answer(base, "search-series-night-of-the-living-dead.json"),
        ),
        ("/meta/movie/tt0063350.json", answer(base, "meta-movie-tt0063350.json")),
        ("/meta/series/tt0063350.json", answer(base, "meta-series-tt0063350.json")),
        ("/meta/movie/tt1727587.json", answer(base, "meta-movie-tt1727587.json")),
        ("/meta/series/tt1727587.json", answer(base, "meta-series-tt1727587.json")),
        ("/meta/series/tt0903747.json", answer(base, "meta-series-tt0903747.json")),
        ("/meta/movie/tt0903747.json", answer(base, "meta-movie-tt0903747.json")),
        ("/images/poster/small/", ok(fixture("poster-small-tt1727587.jpg"))),
    ]
}

fn scratch_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("torrential-catalog-{name}-{}-{nanos}", std::process::id()))
}

/// Moves every file under `dir` back in time by `by`, as if it had been kept that long.
fn age_files(dir: &Path, by: Duration) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            age_files(&path, by);
        } else {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - by).unwrap();
        }
    }
}

fn listed(cards: &[TitleCard]) -> Vec<(&str, Kind, &str, Option<u16>)> {
    cards.iter().map(|c| (c.id.as_str(), c.kind, c.name.as_str(), c.year)).collect()
}

#[tokio::test]
async fn a_search_puts_the_titles_with_that_exact_name_first_in_cinemetas_order() {
    let server = Server::recorded().await;
    let catalog = server.catalog(true, None);
    let found = catalog.search("  night of the living   DEAD ").await.unwrap();
    let night = "Night of the Living Dead";
    assert_eq!(
        listed(&found[..11]),
        [
            ("tt0063350", Kind::Film, night, Some(1968)),
            ("tt49275788", Kind::Series, night, None),
            ("tt0100258", Kind::Film, night, Some(1990)),
            ("tt33296064", Kind::Film, night, Some(2026)),
            ("tt3393372", Kind::Film, night, Some(2014)),
            ("tt13742126", Kind::Film, night, Some(2021)),
            ("tt10675496", Kind::Film, night, None),
            ("tt23054560", Kind::Film, night, None),
            ("tt31435652", Kind::Series, "Nyaight of the Living Cat", Some(2025)),
            ("tt4928166", Kind::Series, "The Living and the Dead", Some(2016)),
            ("tt0489244", Kind::Film, "Night of the Living Dead 3D", Some(2006)),
        ]
    );
    assert_eq!(found.len(), 40, "twenty films and twenty series");
    let mut asked = server.asked();
    asked.sort();
    assert_eq!(
        asked,
        [
            "/catalog/movie/top/search=night%20of%20the%20living%20dead.json",
            "/catalog/series/top/search=night%20of%20the%20living%20dead.json",
        ]
    );
    assert_eq!(server.agents.lock().unwrap().clone(), ["mozilla/5.0"; 2], "no request names the program");

    let namesakes = catalog.namesakes(&found[2]).await.unwrap();
    let ids: Vec<&str> = namesakes.iter().map(|card| card.id.as_str()).collect();
    assert_eq!(ids, ["tt0063350", "tt0100258", "tt33296064", "tt3393372", "tt13742126", "tt10675496", "tt23054560"]);
    assert_eq!(server.asked().len(), 3, "namesakes of a film search only films");
}

#[tokio::test]
async fn a_films_details_read_as_cinemeta_describes_it() {
    let server = Server::recorded().await;
    let catalog = server.catalog(true, Some(&scratch_dir("film")));
    let film = catalog.title(&id(NOTLD)).await.unwrap();
    assert_eq!(listed(std::slice::from_ref(&film.card)), [(NOTLD, Kind::Film, "Night of the Living Dead", Some(1968))]);
    assert_eq!((film.card.rating, film.runtime_min), (Some(7.8), Some(96)));
    assert_eq!(film.cast, ["Duane Jones", "Judith O'Dea", "Karl Hardman"]);
    assert_eq!(film.genres, ["Horror", "Thriller"]);
    assert_eq!(film.countries, Countries::from([Country::Us]));
    assert_eq!(film.synopsis.as_deref().map(|s| &s[..45]), Some("A ragtag group of Pennsylvanians barricade th"));
    assert_eq!((film.art.poster, film.art.background, film.art.logo), (true, true, true));
    assert_eq!(film.seasons, []);
    assert_eq!(
        server.asked(),
        ["/meta/series/tt0063350.json", "/meta/movie/tt0063350.json"],
        "a title of unknown kind is asked for as a series first"
    );

    let short = catalog.title(&id(SINTEL)).await.unwrap();
    assert_eq!((short.card.name.as_str(), short.card.year, short.runtime_min), ("Sintel", Some(2010), Some(14)));
    assert_eq!((short.card.kind, short.card.rating), (Kind::Film, Some(7.6)));
    assert_eq!(short.genres, ["Animation", "Short", "Fantasy"]);
    assert_eq!(short.countries, Countries::default(), "the Netherlands has no code in release names");
    assert_eq!(catalog.title(&id(NOTLD)).await.unwrap(), film, "kept");
    assert_eq!(server.asked().len(), 4, "a kept film is not asked for again");
}

#[tokio::test]
async fn a_series_details_list_its_seasons_with_specials_last() {
    let server = Server::recorded().await;
    let series = server.catalog(true, None).title(&id(BREAKING_BAD)).await.unwrap();
    assert_eq!(listed(std::slice::from_ref(&series.card)), [(BREAKING_BAD, Kind::Series, "Breaking Bad", Some(2008))]);
    assert_eq!((series.card.rating, series.runtime_min), (Some(9.5), Some(49)));
    assert_eq!(series.cast, ["Bryan Cranston", "Aaron Paul", "Anna Gunn"]);
    assert_eq!(series.countries, Countries::from([Country::Us]));
    let seasons: Vec<(u32, usize)> = series.seasons.iter().map(|s| (s.number, s.episodes.len())).collect();
    assert_eq!(seasons, [(1, 7), (2, 13), (3, 13), (4, 13), (5, 16), (0, 5)]);
    let numbers: Vec<u32> = series.seasons[0].episodes.iter().map(|e| e.episode.episode).collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5, 6, 7]);
    let second = &series.seasons[0].episodes[1];
    assert_eq!(second.episode, Episode { season: 1, episode: 2 });
    assert_eq!(second.name.as_deref(), Some("Cat's in the Bag..."));
    assert_eq!(second.aired, Some(1_201_496_400), "2008-01-28 05:00 UTC");
    assert_eq!(
        second.overview.as_deref(),
        Some(
            "Walt and Jesse attempt to tie up loose ends. The desperate situation gets more complicated with the \
             flip of a coin. Walt's wife, Skyler, becomes suspicious of Walt's strange behavior."
        )
    );
    assert!(second.thumbnail);
    let special = &series.seasons[5].episodes[0];
    assert_eq!((special.name.as_deref(), special.aired), (Some("Good Cop Bad Cop"), Some(1_234_933_200)));
    assert_eq!(server.asked(), ["/meta/series/tt0903747.json"], "Cinemeta's wrong film for this ID is never asked for");
}

#[tokio::test]
async fn a_picture_is_fetched_once_and_then_read_from_disk() {
    let server = Server::recorded().await;
    let dir = scratch_dir("pictures");
    let catalog = server.catalog(true, Some(&dir));
    let poster = fixture("poster-small-tt1727587.jpg");
    for _ in 0..2 {
        let picture = catalog.art(&id(SINTEL), ArtKind::Poster).await.unwrap();
        assert_eq!((picture.bytes.as_slice(), picture.format), (poster.as_slice(), ImageFormat::Jpeg));
    }
    let pictures = server.asked().into_iter().filter(|path| path.starts_with("/images/")).count();
    assert_eq!(pictures, 1, "{:?}", server.asked());

    let before = server.asked().len();
    let again = server.catalog(true, Some(&dir)).art(&id(SINTEL), ArtKind::Poster).await.unwrap();
    assert_eq!(again.bytes.len(), poster.len());
    assert_eq!(server.asked().len(), before, "another catalog on the same folder asks no one");

    catalog.search("night of the living dead").await.unwrap();
    let before = server.asked().len();
    catalog.art(&id(NOTLD), ArtKind::Poster).await.unwrap();
    assert_eq!(
        server.asked()[before..],
        ["/images/poster/small/tt0063350/img"],
        "a searched title's poster needs no details"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn turned_off_it_asks_no_one_and_answers_from_disk() {
    let server = Server::recorded().await;
    let dir = scratch_dir("offline");
    let offline = server.catalog(false, Some(&dir));
    assert_eq!(offline.search("night of the living dead").await.unwrap(), []);
    let error = offline.title(&id(NOTLD)).await.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::Usage, "title lookups are off in config.yaml, and tt0063350 is not kept on this machine")
    );
    let error = offline.art(&id(NOTLD), ArtKind::Poster).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Usage);
    let card = TitleCard {
        id: id(NOTLD),
        kind: Kind::Film,
        name: "Night of the Living Dead".into(),
        year: None,
        rating: None,
    };
    assert_eq!(offline.namesakes(&card).await.unwrap(), []);
    assert_eq!(server.asked(), Vec::<String>::new());

    let online = server.catalog(true, Some(&dir)).title(&id(NOTLD)).await.unwrap();
    let asked = server.asked().len();
    age_files(&dir, Duration::from_secs(60 * 24 * 60 * 60));
    assert_eq!(offline.title(&id(NOTLD)).await.unwrap(), online, "a stale copy answers when turned off");
    assert_eq!(server.asked().len(), asked);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_stale_copy_answers_when_cinemeta_is_down() {
    let up = Server::recorded().await;
    let dir = scratch_dir("stale");
    let kept = up.catalog(true, Some(&dir)).title(&id(BREAKING_BAD)).await.unwrap();
    let down = Server::start(|_| vec![("/", status(503))]).await;
    assert_eq!(down.catalog(true, Some(&dir)).title(&id(BREAKING_BAD)).await.unwrap(), kept);
    assert_eq!(down.asked(), Vec::<String>::new(), "a series kept today is fresh");

    age_files(&dir, Duration::from_secs(2 * 24 * 60 * 60));
    assert_eq!(down.catalog(true, Some(&dir)).title(&id(BREAKING_BAD)).await.unwrap(), kept);
    assert!(down.asked().iter().all(|path| path == "/meta/series/tt0903747.json"), "{:?}", down.asked());
    assert!(!down.asked().is_empty(), "a series kept two days ago is asked for again");

    let error = down.catalog(true, Some(&dir)).title(&id(NOTLD)).await.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::FetchFailed, "cannot look up tt0063350 on Cinemeta: HTTP 503")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_picture_that_is_not_an_image_or_is_too_large_is_refused_and_remembered() {
    let mut too_large = vec![0xff, 0xd8, 0xff];
    too_large.resize((5 << 20) + 1, 0);
    let server = Server::start(|base| {
        vec![
            ("/meta/movie/tt1727587.json", answer(base, "meta-movie-tt1727587.json")),
            ("/meta/series/tt1727587.json", answer(base, "meta-series-tt1727587.json")),
            ("/images/poster/", ok(b"<html>a page</html>".to_vec())),
            ("/images/background/", ok(too_large)),
        ]
    })
    .await;
    let catalog = server.catalog(true, None);
    let refused = async |kind| {
        let error = catalog.art(&id(SINTEL), kind).await.unwrap_err();
        (error.code, error.message)
    };
    for _ in 0..2 {
        assert_eq!(
            refused(ArtKind::Poster).await,
            (ErrorCode::NotFound, "the poster for tt1727587 is not a JPEG, PNG, or WebP picture".into())
        );
        assert_eq!(
            refused(ArtKind::Background).await,
            (ErrorCode::NotFound, "the background for tt1727587 is larger than 5 MiB".into())
        );
        assert_eq!(
            refused(ArtKind::Logo).await,
            (ErrorCode::NotFound, "the logo for tt1727587 is gone: HTTP 404".into())
        );
    }
    let pictures: Vec<String> = server.asked().into_iter().filter(|path| path.starts_with("/images/")).collect();
    assert_eq!(
        pictures,
        [
            "/images/poster/small/tt1727587/img",
            "/images/background/medium/tt1727587/img",
            "/images/logo/medium/tt1727587/img",
        ]
    );
}

#[tokio::test]
async fn a_picture_not_at_an_https_address_is_refused() {
    let server = Server::start(|base| {
        let mut routes = recorded(base);
        let elsewhere = base.replace("127.0.0.1", "localhost");
        let moved = Reply { status: 302, body: Vec::new(), location: Some(format!("{elsewhere}{SINTEL_POSTER}")) };
        routes.push(("/images/background/", moved));
        routes
    })
    .await;
    let config = CatalogConfig { online: true, endpoint: Some(server.base.clone()), cache_dir: None };
    let strict = Catalog::new(&config).unwrap();
    let error = strict.art(&id(SINTEL), ArtKind::Poster).await.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::NotFound, "the poster for tt1727587 is not at an https address")
    );
    assert!(server.asked().iter().all(|path| path.starts_with("/meta/")), "{:?}", server.asked());

    let local = server.catalog(true, None);
    assert_eq!(local.art(&id(SINTEL), ArtKind::Poster).await.unwrap().format, ImageFormat::Jpeg);
    let error = local.art(&id(SINTEL), ArtKind::Background).await.unwrap_err();
    assert_eq!(error.message, "the background for tt1727587 is not at an https address");
    let posters = server.asked().into_iter().filter(|path| path == SINTEL_POSTER).count();
    assert_eq!(posters, 1, "a redirect to another plain HTTP origin is not followed");
}
