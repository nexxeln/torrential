//! Online subtitles against a local server that answers with what IMDb and the OpenSubtitles
//! add-on answered when the fixtures were recorded.

use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrential_core::subtitle;
use torrential_core::title::{self, Episode, Title};
use torrential_search::subtitles::{FileHash, Found, Online, SubtitleConfig, Video};

const SINTEL: &str = "/os/subtitles/movie/tt1727587";
const SINTEL_SRT: &str = "/en/download/subencoding-stremio-utf8/src-api/file/1952537611";

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

/// Serves each path that starts with a route's prefix, and remembers every path asked for.
struct Server {
    base: String,
    asked: Arc<Mutex<Vec<String>>>,
    /// The `User-Agent` of each request.
    agents: Arc<Mutex<Vec<String>>>,
}

impl Server {
    async fn start(routes: Vec<(&'static str, u16, String)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let agents: Arc<Mutex<Vec<String>>> = Arc::default();
        let (seen, named, own) = (asked.clone(), agents.clone(), base.clone());
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
                let (status, body) = match routes.iter().find(|(prefix, _, _)| path.starts_with(prefix)) {
                    // The add-on's listing says where each subtitle is. Here that is this server.
                    Some((_, status, body)) => (*status, body.replace("https://subs5.strem.io", &own)),
                    None => (404, String::new()),
                };
                let head =
                    format!("HTTP/1.1 {status} X\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len());
                let _ = socket.write_all(format!("{head}{body}").as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        Server { base, asked, agents }
    }

    /// What the sources answered for Sintel and for an episode of Breaking Bad.
    async fn recorded() -> Server {
        Server::start(vec![
            ("/imdb/suggestion/x/sintel", 200, fixture("imdb/suggestion-sintel.json")),
            ("/imdb/suggestion/x/breaking%20bad", 200, fixture("imdb/suggestion-breaking-bad.json")),
            (SINTEL, 200, fixture("opensubtitles/subtitles-movie-tt1727587.json")),
            ("/os/subtitles/series/tt0903747:3:5", 200, fixture("opensubtitles/subtitles-series-tt0903747-3-5.json")),
            (SINTEL_SRT, 200, fixture("opensubtitles/sintel_en.srt")),
        ])
        .await
    }

    fn online(&self, online: bool) -> Online {
        let endpoints = [("imdb", "imdb"), ("opensubtitles", "os")];
        let endpoints = endpoints.map(|(name, path)| (name.to_string(), format!("{}/{path}", self.base)));
        Online::new(&SubtitleConfig { online, endpoints: endpoints.into() }).unwrap()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

fn video(file: &str, hash: Option<FileHash>) -> Video {
    Video { titles: title::read(file, ""), file_name: file.to_string(), hash }
}

/// Each subtitle as its ID, language, and whether it is exact.
fn listed(found: &Found) -> Vec<(&str, &str, bool)> {
    found.subtitles.iter().map(|s| (s.id.as_str(), s.language.as_str(), s.exact)).collect()
}

#[tokio::test]
async fn a_film_is_found_by_its_name_and_its_subtitle_downloads() {
    let server = Server::recorded().await;
    let online = server.online(true);
    let found = online.find(&video("Sintel.2010.1080p.mkv", None)).await.expect("the sources answered");
    assert_eq!(found.title, Some(Title { name: "Sintel".into(), year: Some(2010), episode: None }));
    assert_eq!(server.asked(), ["/imdb/suggestion/x/sintel.json", "/os/subtitles/movie/tt1727587.json"]);
    assert_eq!(
        listed(&found)[..6],
        [
            ("3871796", "eng", false),
            ("6738581", "eng", false),
            ("4784578", "eng", false),
            ("3863890", "eng", false),
            ("3862663", "ell", false),
            ("4164726", "ell", false),
        ],
        "each language together, in the order the source ranks them"
    );
    let names: Vec<Option<&str>> = found.subtitles[..2].iter().map(|s| s.name.as_deref()).collect();
    assert_eq!(names, [Some("sintel_en.srt"), Some("Sintel (2010)")], "the release name, or else the file name");
    let greek = found.subtitles.iter().filter(|s| s.language == "ell").count();
    assert_eq!(greek, 5, "the source lists more Greek subtitles than anyone needs");
    assert!(found.subtitles.iter().all(|s| s.source == "opensubtitles"));

    let srt = online.fetch(&found.subtitles[0]).await.unwrap();
    let vtt = subtitle::to_vtt(&subtitle::decode(&srt)).unwrap();
    assert!(vtt.starts_with("WEBVTT\n\n00:01:47.250 --> 00:01:50.500\nThis blade has a dark past.\n\n"), "{vtt}");
    assert_eq!(server.asked().last().map(String::as_str), Some(SINTEL_SRT));
    let agents = server.agents.lock().unwrap().clone();
    assert_eq!(agents, ["mozilla/5.0"; 3], "no request names the program or the machine");
}

#[tokio::test]
async fn a_files_hash_goes_with_the_search_and_its_matches_come_first() {
    let listing = fixture("opensubtitles/subtitles-movie-tt1727587.json");
    // As the add-on marks a subtitle timed for this exact file.
    let by_hash = listing.replacen(
        r#""lang":"eng","m":"i","g":"1","subtitleFileName":"sintel.he.srt""#,
        r#""lang":"eng","m":"h","g":"1","subtitleFileName":"sintel.he.srt""#,
        1,
    );
    assert_ne!(by_hash, listing, "the fixture still lists sintel.he.srt");
    let server = Server::start(vec![
        ("/imdb/suggestion/x/sintel", 200, fixture("imdb/suggestion-sintel.json")),
        (SINTEL, 200, by_hash),
    ])
    .await;
    let hash = FileHash { hash: 0x8e24_5d96_79d3_1e12, size: 12_909_756 };
    let found = server.online(true).find(&video("Sintel (2010).mkv", Some(hash))).await.unwrap();
    assert_eq!(
        server.asked()[1],
        "/os/subtitles/movie/tt1727587/videoHash=8e245d9679d31e12&videoSize=12909756&filename=Sintel%20%282010%29%2Emkv.json"
    );
    assert_eq!(listed(&found)[..3], [("4784578", "eng", true), ("3871796", "eng", false), ("6738581", "eng", false)]);
}

#[tokio::test]
async fn an_episode_is_searched_by_its_series_season_and_number() {
    let server = Server::recorded().await;
    let found = server.online(true).find(&video("Breaking.Bad.S03E05.720p.HDTV.x264.mkv", None)).await.unwrap();
    let episode = Some(Episode { season: 3, episode: 5 });
    assert_eq!(found.title, Some(Title { name: "Breaking Bad".into(), year: Some(2008), episode }));
    assert_eq!(server.asked(), ["/imdb/suggestion/x/breaking%20bad.json", "/os/subtitles/series/tt0903747:3:5.json"]);
    let english = found.subtitles.iter().take_while(|s| s.language == "eng").count();
    assert_eq!(english, 5, "the source lists six");
}

#[tokio::test]
async fn a_name_imdb_does_not_know_exactly_finds_nothing_and_searches_nothing() {
    let server = Server::recorded().await;
    let online = server.online(true);
    for file in ["Sintel.2.2012.1080p.mkv", "Sintel.1999.mkv", "Sintel.S01E01.mkv"] {
        assert_eq!(online.find(&video(file, None)).await, Some(Found::default()), "{file}");
    }
    assert!(server.asked().iter().all(|path| path.starts_with("/imdb/")), "{:?}", server.asked());
    assert_eq!(
        online.find(&Video { titles: Vec::new(), file_name: "a.mkv".into(), hash: None }).await,
        Some(Found::default())
    );
}

#[tokio::test]
async fn a_source_that_does_not_answer_is_no_answer_and_not_an_empty_one() {
    let down = "the server is down".to_string();
    let imdb_down = Server::start(vec![("/imdb/", 503, down.clone())]).await;
    assert_eq!(imdb_down.online(true).find(&video("Sintel.2010.mkv", None)).await, None);
    let provider_down = Server::start(vec![
        ("/imdb/suggestion/x/sintel", 200, fixture("imdb/suggestion-sintel.json")),
        (SINTEL, 503, down.clone()),
        (SINTEL_SRT, 503, down),
    ])
    .await;
    let online = provider_down.online(true);
    assert_eq!(online.find(&video("Sintel.2010.mkv", None)).await, None);
    let recorded = Server::recorded().await;
    let mut subtitle = recorded.online(true).find(&video("Sintel.2010.mkv", None)).await.unwrap().subtitles.remove(0);
    subtitle.url = subtitle.url.replace(&recorded.base, &provider_down.base);
    let error = online.fetch(&subtitle).await.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (torrential_core::ErrorCode::FetchFailed, "cannot download the subtitle from opensubtitles: HTTP 503")
    );
    subtitle.url = "file:///etc/passwd".into();
    assert!(online.fetch(&subtitle).await.unwrap_err().message.ends_with("its address is not a web address"));
}

#[tokio::test]
async fn turned_off_it_finds_nothing_and_asks_no_one() {
    let server = Server::recorded().await;
    assert_eq!(server.online(false).find(&video("Sintel.2010.1080p.mkv", None)).await, Some(Found::default()));
    assert_eq!(server.asked(), Vec::<String>::new());
}
