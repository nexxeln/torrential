//! The web app's side of the engine's HTTP server: its pages, its API, and what it asks ffmpeg for.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use torrential_core::{Display, Playback, Preferences, Screen, Target};

use super::super::admission::GIB;
use super::super::engine_port::FakeTorrentFile;
use super::super::http;
use super::super::search_port::{self, FakeSource, SearchPort, fake_catalog};
use super::super::web::ffmpeg::Tools;
use super::{Harness, MIB, Outside, magnet, prefix, titles, torrent, written};

/// One HTTP/1.1 connection from a browser, or from curl.
struct Page(BufReader<TcpStream>);

impl Page {
    async fn open(h: &Harness) -> Self {
        Self(BufReader::new(TcpStream::connect(("127.0.0.1", h.running.ctx.http_port)).await.unwrap()))
    }

    /// A connection the engine takes as from `remote`, as it takes a phone's on the home network.
    async fn open_from(h: &Harness, remote: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        http::spawn(h.running.ctx.clone(), stream, remote.parse().unwrap());
        Self(BufReader::new(client))
    }

    async fn send(&mut self, method: &str, path: &str, headers: &[&str], body: &str) {
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
        for header in headers {
            request.push_str(&format!("{header}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body);
        self.0.get_mut().write_all(request.as_bytes()).await.unwrap();
    }

    /// The status and the headers, lowercased, sorted, and one per line.
    async fn head(&mut self) -> (u16, String) {
        let mut line = String::new();
        self.0.read_line(&mut line).await.unwrap();
        let status = line.split(' ').nth(1).unwrap_or_else(|| panic!("no status in {line:?}")).parse().unwrap();
        let mut headers = Vec::new();
        loop {
            line.clear();
            self.0.read_line(&mut line).await.unwrap();
            let Some((name, value)) = line.trim_end().split_once(": ") else { break };
            headers.push(format!("{}: {value}\n", name.to_ascii_lowercase()));
        }
        headers.sort();
        (status, headers.concat())
    }

    /// The next chunk of a streamed answer, or `None` at its end.
    async fn chunk(&mut self) -> Option<Vec<u8>> {
        let mut size = String::new();
        self.0.read_line(&mut size).await.unwrap();
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or_else(|_| panic!("the answer was cut short"));
        let mut chunk = vec![0; size + 2];
        self.0.read_exact(&mut chunk).await.unwrap();
        chunk.truncate(size);
        (size > 0).then_some(chunk)
    }

    /// The next server-sent event.
    async fn event(&mut self) -> Value {
        let chunk = tokio::time::timeout(Duration::from_secs(10), self.chunk()).await.expect("an event arrives");
        let chunk = String::from_utf8(chunk.expect("the stream stays open")).unwrap();
        let event = chunk.strip_prefix("data: ").and_then(|c| c.strip_suffix("\n\n")).expect("one event a chunk");
        serde_json::from_str(event).unwrap()
    }

    async fn body(&mut self, headers: &str) -> Vec<u8> {
        let mut body = Vec::new();
        match headers.lines().find_map(|h| h.strip_prefix("content-length: ")) {
            Some(length) => {
                body.resize(length.parse().unwrap(), 0);
                self.0.read_exact(&mut body).await.unwrap();
            }
            None => {
                while let Some(chunk) = self.chunk().await {
                    body.extend(chunk);
                }
            }
        }
        body
    }
}

/// One request on its own connection: the status, the headers, and the whole body.
async fn fetch(h: &Harness, method: &str, path: &str, headers: &[&str], body: &str) -> (u16, String, Vec<u8>) {
    exchange(Page::open(h).await, method, path, headers, body).await
}

/// `fetch`, on a connection the engine takes as from `remote`.
async fn fetch_from(
    h: &Harness,
    remote: &str,
    method: &str,
    path: &str,
    headers: &[&str],
    body: &str,
) -> (u16, String, Vec<u8>) {
    exchange(Page::open_from(h, remote).await, method, path, headers, body).await
}

async fn exchange(mut page: Page, method: &str, path: &str, headers: &[&str], body: &str) -> (u16, String, Vec<u8>) {
    page.send(method, path, headers, body).await;
    let (status, headers) = page.head().await;
    let body = if method == "HEAD" { Vec::new() } else { page.body(&headers).await };
    (status, headers, body)
}

async fn get(h: &Harness, path: &str) -> (u16, Value) {
    let (status, _, body) = fetch(h, "GET", path, &[], "").await;
    (status, serde_json::from_slice(&body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&body))))
}

async fn post(h: &Harness, path: &str, body: Value) -> (u16, Value) {
    let (status, _, body) = fetch(h, "POST", path, &["Content-Type: application/json"], &body.to_string()).await;
    (status, serde_json::from_slice(&body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&body))))
}

/// The error an answer carries, as its status, code, and message.
fn error((status, body): &(u16, Value)) -> (u16, &str, &str) {
    (*status, body["error"]["code"].as_str().unwrap_or_default(), body["error"]["message"].as_str().unwrap_or_default())
}

fn magnet_link(torrent: &FakeTorrentFile) -> String {
    let Target::Magnet(link) = magnet(torrent) else { unreachable!() };
    link
}

#[tokio::test(flavor = "multi_thread")]
async fn the_app_answers_for_its_pages_and_not_for_missing_scripts() {
    let h = Harness::new();
    let (status, headers, page) = fetch(&h, "GET", "/", &[], "").await;
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: text/html; charset=utf-8\n"), "{headers}");
    assert!(headers.contains("cache-control: no-cache\n"), "{headers}");
    assert!(page.starts_with(b"<!doctype html>"), "{}", String::from_utf8_lossy(&page));
    assert_eq!(fetch(&h, "GET", "/watch/9b5212?file=2", &[], "").await.2, page);
    assert_eq!(fetch(&h, "GET", "/downloads", &[], "").await.2, page);
    let (status, headers, body) = fetch(&h, "HEAD", "/", &[], "").await;
    assert_eq!((status, body.len()), (200, 0));
    assert!(headers.contains(&format!("content-length: {}\n", page.len())), "{headers}");
    assert_eq!(fetch(&h, "GET", "/assets/index-gone.js", &[], "").await.0, 404);
    assert_eq!(fetch(&h, "POST", "/", &["Content-Type: application/json"], "{}").await.0, 405);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_websites_cannot_use_the_engine_through_the_browser() {
    let h = Harness::new();
    let port = h.running.ctx.http_port;
    let status = |headers: Vec<String>| {
        let h = &h;
        async move {
            let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
            fetch(h, "GET", "/api/jobs", &headers, "").await.0
        }
    };
    assert_eq!(status(vec![]).await, 200, "curl, or an agent");
    assert_eq!(status(vec!["Sec-Fetch-Site: same-origin".into()]).await, 200);
    assert_eq!(status(vec![format!("Origin: http://localhost:{port}")]).await, 200);
    assert_eq!(status(vec!["Sec-Fetch-Site: cross-site".into()]).await, 403);
    assert_eq!(status(vec!["Origin: https://example.org".into()]).await, 403);
    assert_eq!(status(vec![format!("Origin: http://127.0.0.1:{}", port.wrapping_add(1))]).await, 403);
    assert_eq!(fetch(&h, "GET", "/", &["Sec-Fetch-Site: cross-site"], "").await.0, 403);

    let body = json!({ "refs": ["9b5212"] }).to_string();
    // A form on another website can send a POST, but not one typed as JSON.
    assert_eq!(fetch(&h, "POST", "/api/remove", &["Content-Type: text/plain"], &body).await.0, 415);
    assert_eq!(fetch(&h, "POST", "/api/remove", &[], &body).await.0, 415);
    assert_eq!(fetch(&h, "PUT", "/api/remove", &["Content-Type: application/json"], &body).await.0, 405);
    let long = json!({ "refs": ["a".repeat(70_000)] }).to_string();
    assert_eq!(fetch(&h, "POST", "/api/remove", &["Content-Type: application/json"], &long).await.0, 413);
    assert_eq!(error(&post(&h, "/api/remove", json!({ "refs": [] })).await), (400, "usage", "refs names no download"));
    assert_eq!(error(&get(&h, "/api/nothing").await), (404, "not_found", "the engine has no GET /api/nothing"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_search_sends_a_line_each_time_a_source_answers() {
    let listed = |hex: &str, name: &str, source: &str| torrential_core::Release {
        id: torrential_core::ReleaseId::Torrent(hex.repeat(20).parse().unwrap()),
        name: name.into(),
        sources: vec![source.into()],
        ..fake_catalog().remove(0)
    };
    let catalog = vec![listed("aa", "dead \u{1b}[31mred", "slow"), listed("bb", "dead fast", "quick")];
    let script = vec![FakeSource::answers("quick", 0), FakeSource::answers("slow", 60)];
    let h = Harness::with_search(search_port::Fake::scripted(catalog, script, Default::default()));
    let (status, headers, body) = fetch(&h, "GET", "/api/search?q=dead&limit=5", &[], "").await;
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: application/x-ndjson\n"), "{headers}");
    let lines: Vec<String> = String::from_utf8(body)
        .unwrap()
        .lines()
        .map(|line| {
            let message: Value = serde_json::from_str(line).unwrap();
            let names: Vec<&str> =
                message["result"]["releases"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
            format!("{} {names:?} pending {}", message["type"].as_str().unwrap(), message["pending"])
        })
        .collect();
    assert_eq!(lines, [r#"update ["dead fast"] pending ["slow"]"#, r#"done ["dead red", "dead fast"] pending null"#,]);
    assert_eq!(
        error(&get(&h, "/api/search?q=%20").await),
        (400, "usage", "the search has no words: the q parameter is empty")
    );
    assert_eq!(error(&get(&h, "/api/search?q=dead&category=films").await), (400, "usage", "films is not a category"));
}

#[tokio::test(flavor = "multi_thread")]
async fn play_picks_the_video_and_its_subtitles_and_says_where_they_are() {
    let h = Harness::new();
    let film =
        torrent(60, "film", &[("Film.mkv", 3 * GIB), ("Film.en.srt", 40_000), ("extras.mkv", GIB), ("notes.txt", 10)]);
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    let (status, playing) = post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    assert_eq!(status, 200, "{playing}");
    let selected = |playing: &Value| -> Vec<bool> {
        playing["files"].as_array().unwrap().iter().map(|f| f["selected"].as_bool().unwrap()).collect()
    };
    assert_eq!(
        json!([playing["id"], playing["name"], playing["total_size"], playing["job"]["id"], playing["job"]["name"]]),
        json!([id, "film", 4 * GIB + 40_010, id, "film"])
    );
    assert_eq!(
        playing["video"],
        json!({ "index": 0, "path": "Film.mkv", "size": 3 * GIB, "url": format!("/{id}/0/Film.mkv"), "offset": 0 })
    );
    assert_eq!(
        playing["subtitles"],
        json!([{ "index": 1, "path": "Film.en.srt", "size": 40_000, "url": format!("/{id}/1/Film.en.srt"), "offset": 3 * GIB }])
    );
    assert_eq!(selected(&playing), [true, true, false, false]);

    let (_, chosen) = post(&h, "/api/play", json!({ "ref": prefix(&film), "file": 2 })).await;
    assert_eq!(json!([chosen["video"]["index"], chosen["video"]["offset"]]), json!([2, 3 * GIB + 40_000]));
    assert_eq!(selected(&chosen), [true, true, true, false]);
    assert_eq!(h.status().await.len(), 1, "both plays are one download");

    let papers = torrent(61, "papers", &[("a.pdf", 1000), ("b.pdf", 1000)]);
    h.swarm.publish(papers.clone());
    let file = h.dir.path().join("film.torrent").display().to_string();
    let refused = [
        post(&h, "/api/play", json!({ "ref": prefix(&film), "file": 9 })).await,
        post(&h, "/api/play", json!({ "ref": magnet_link(&papers) })).await,
        post(&h, "/api/play", json!({ "ref": "c0ffee" })).await,
        post(&h, "/api/play", json!({ "ref": file })).await,
        post(&h, "/api/play", json!({ "file": 1 })).await,
    ];
    let refused: Vec<(u16, &str)> = refused.iter().map(error).map(|(status, code, _)| (status, code)).collect();
    assert_eq!(
        refused,
        [(400, "usage"), (404, "no_selection"), (404, "not_found"), (404, "invalid_ref"), (400, "usage")]
    );
    assert_eq!(h.status().await.len(), 1, "a torrent with nothing to play starts no download");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_follows_and_changes_downloads() {
    let h = Harness::new();
    assert_eq!(get(&h, "/api/jobs").await, (200, json!({ "jobs": [] })));
    let mut events = Page::open(&h).await;
    events.send("GET", "/api/events", &[], "").await;
    let (status, headers) = events.head().await;
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: text/event-stream\n"), "{headers}");
    assert_eq!(events.event().await, json!({ "event": "snapshot", "jobs": [] }));

    let film = torrent(62, "film", &[("film.mkv", GIB), ("film.srt", 40_000)]);
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    let event = events.event().await;
    assert_eq!(json!([event["event"], event["job"]["id"]]), json!(["job", id]));
    let (_, listing) = get(&h, &format!("/api/files?ref={}", prefix(&film))).await;
    assert_eq!(
        json!([listing["id"], listing["files"][1]["path"], listing["files"][1]["selected"]]),
        json!([id, "film.srt", true])
    );
    assert_eq!(get(&h, "/api/jobs").await.1["jobs"][0]["id"], json!(id));

    let change = |body: (u16, Value)| json!([body.0, body.1["changes"][0]["id"], body.1["changes"][0]["result"]]);
    let refs = json!({ "refs": [prefix(&film)] });
    assert_eq!(change(post(&h, "/api/pause", refs.clone()).await), json!([200, id, "paused"]));
    assert_eq!(change(post(&h, "/api/resume", refs.clone()).await), json!([200, id, "resumed"]));
    assert_eq!(change(post(&h, "/api/remove", refs.clone()).await), json!([200, id, "removed"]));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let event = events.event().await;
        if event == json!({ "event": "removed", "id": id }) {
            break;
        }
        assert!(Instant::now() < deadline, "the page never heard the download was removed");
    }
    assert_eq!(error(&post(&h, "/api/pause", refs).await).0, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_ffmpeg_a_file_goes_by_its_name_and_its_header() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let clip = include_bytes!("../../../../core/tests/fixtures/media/faststart.mp4");
    let film = torrent(63, "film", &[("film.mp4", clip.len() as u64), ("film.mkv", 5_000)]);
    std::fs::create_dir_all(h.downloads().join("film")).unwrap();
    std::fs::write(h.downloads().join("film/film.mp4"), clip).unwrap();
    written(&h.downloads().join("film/film.mkv"), 5_000);
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    post(&h, "/api/play", json!({ "ref": magnet_link(&film), "file": 0 })).await;
    post(&h, "/api/play", json!({ "ref": prefix(&film), "file": 1 })).await;
    let (status, media) = get(&h, &format!("/api/media?id={id}&file=0")).await;
    assert_eq!(status, 200, "{media}");
    assert_eq!(
        media,
        json!({
            "duration": 7.0, "container": "mp4", "video": null, "audio": [], "subtitles": [],
            "direct": "video/mp4", "remux": false, "chapters": [],
        })
    );
    let (_, media) = get(&h, &format!("/api/media?id={id}&file=1")).await;
    assert_eq!(json!([media["duration"], media["container"], media["direct"]]), json!([null, "matroska", null]));

    let needs_ffmpeg = [
        get(&h, &format!("/api/stream?id={id}&file=0&t=0&video=copy")).await,
        get(&h, &format!("/api/subtitles?id={id}&file=0&stream=2&t=0")).await,
    ];
    for answer in &needs_ffmpeg {
        assert_eq!(error(answer), (404, "not_found", "ffmpeg is not installed, and this needs it"));
    }
    let not_playing = [
        get(&h, &format!("/api/media?id={id}&file=7")).await,
        get(&h, &format!("/api/media?id={}&file=0", "ab".repeat(20))).await,
        get(&h, "/api/media?id=film&file=0").await,
        get(&h, &format!("/api/media?id={id}")).await,
    ];
    let codes: Vec<(u16, &str)> = not_playing.iter().map(error).map(|(status, code, _)| (status, code)).collect();
    assert_eq!(codes, [(404, "not_found"), (404, "not_found"), (404, "invalid_ref"), (400, "usage")]);
}

/// Programs that stand in for ffmpeg and ffprobe. The stand-in ffprobe prints `probe`, and each
/// appends its arguments to `calls`.
struct StandIns {
    dir: PathBuf,
}

impl StandIns {
    fn install(h: &Harness, probe: &str, ffmpeg: &str) -> Self {
        let dir = h.dir.path().join("tools");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("probe.json"), probe).unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            let calls = dir.join("calls").display().to_string();
            std::fs::write(&path, format!("#!/bin/sh\necho \"{name} $*\" >> '{calls}'\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let tools = Tools {
            ffprobe: Some(script("ffprobe", &format!("cat '{}'", dir.join("probe.json").display()))),
            ffmpeg: Some(script("ffmpeg", ffmpeg)),
        };
        *h.outside.tools.lock().unwrap() = tools;
        Self { dir }
    }

    fn calls(&self) -> Vec<String> {
        let calls = std::fs::read_to_string(self.dir.join("calls")).unwrap_or_default();
        calls.lines().map(String::from).collect()
    }
}

const FILM: &str = r#"{"streams":[
    {"index":0,"codec_name":"hevc","profile":"Main 10","codec_type":"video","width":1920,"height":1080,
     "pix_fmt":"yuv420p10le","level":120,"avg_frame_rate":"24/1"},
    {"index":1,"codec_name":"eac3","codec_type":"audio","channels":6,"disposition":{"default":1}},
    {"index":2,"codec_name":"aac","codec_type":"audio","channels":2},
    {"index":3,"codec_name":"subrip","codec_type":"subtitle"},
    {"index":4,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle"}
  ],"format":{"format_name":"matroska,webm","duration":"5400.0"}}"#;

async fn playing(h: &Harness, n: u8) -> String {
    let film = torrent(n, "film", &[("Film (2024).mkv", GIB), ("Film.srt", 40_000)]);
    h.swarm.publish(film.clone());
    let (status, playing) = post(h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    assert_eq!(status, 200, "{playing}");
    film.info_hash.id().to_hex()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_player_gets_a_files_tracks_once_and_a_stream_of_what_ffmpeg_writes() {
    let h = Harness::new();
    let tools = StandIns::install(&h, FILM, "printf 'ftyp'; printf 'moof'");
    let id = playing(&h, 64).await;
    let port = h.running.ctx.http_port;
    let url = format!("http://127.0.0.1:{port}/{id}/0/Film%20%282024%29.mkv");
    let (status, media) = get(&h, &format!("/api/media?id={id}&file=0")).await;
    assert_eq!(status, 200, "{media}");
    assert_eq!(
        json!([
            media["duration"],
            media["container"],
            media["video"]["mime"],
            media["audio"][0]["codec"],
            media["remux"]
        ]),
        json!([5400.0, "matroska", "hvc1.2.4.L120.B0", "eac3", true])
    );
    assert_eq!(get(&h, &format!("/api/media?id={id}&file=0")).await.1, media);
    assert_eq!(
        tools.calls(),
        [format!("ffprobe -v error -print_format json -show_format -show_streams -show_chapters {url}")],
        "the second answer is remembered"
    );

    let (status, headers, body) =
        fetch(&h, "GET", &format!("/api/stream?id={id}&file=0&t=3000.5&audio=2&video=copy"), &[], "").await;
    assert_eq!((status, body.as_slice()), (200, &b"ftypmoof"[..]));
    assert_eq!(
        headers,
        "cache-control: no-store\nconnection: close\ncontent-type: video/mp4\ntransfer-encoding: chunked\n"
    );
    assert_eq!(
        tools.calls()[1],
        format!(
            "ffmpeg -v error -nostdin -ss 3000.500 -copyts -i {url} -map 0:0 -c:v copy -tag:v hvc1 -map 0:2 -c:a copy \
             -map_chapters -1 -map_metadata -1 -avoid_negative_ts disabled -movflags \
             frag_keyframe+empty_moov+default_base_moof+frag_discont -f mp4 pipe:1"
        )
    );
    let (status, headers, _) = fetch(&h, "GET", &format!("/api/subtitles?id={id}&file=0&stream=3&t=60"), &[], "").await;
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: text/vtt; charset=utf-8\n"), "{headers}");
    assert_eq!(
        tools.calls()[2],
        format!(
            "ffmpeg -v error -nostdin -ss 60.000 -copyts -i {url} -map 0:3 -c:s webvtt -avoid_negative_ts disabled \
             -flush_packets 1 -f webvtt pipe:1"
        )
    );

    let refused = [
        get(&h, &format!("/api/stream?id={id}&file=0&audio=3")).await,
        get(&h, &format!("/api/stream?id={id}&file=0&video=vp9")).await,
        get(&h, &format!("/api/stream?id={id}&file=0&t=soon")).await,
        get(&h, &format!("/api/subtitles?id={id}&file=0&stream=4")).await,
        get(&h, &format!("/api/subtitles?id={id}&file=0&stream=9")).await,
    ];
    let refused: Vec<(u16, &str, &str)> = refused.iter().map(error).collect();
    assert_eq!(
        refused,
        [
            (400, "usage", "Film (2024).mkv has no audio track at stream 3"),
            (400, "usage", "video=vp9 is not copy or h264"),
            (400, "usage", "t=soon is not a number this route takes"),
            (400, "usage", "stream 4 of Film (2024).mkv holds pictures, not text"),
            (404, "not_found", "Film (2024).mkv has no subtitle track at stream 9"),
        ]
    );
    assert_eq!(tools.calls().len(), 3, "a refused request runs nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn ffmpeg_stops_when_the_player_stops_reading() {
    let h = Harness::new();
    let pid = h.dir.path().join("pid");
    StandIns::install(&h, FILM, &format!("echo $$ > '{}'; printf 'ftyp'; exec sleep 600", pid.display()));
    let id = playing(&h, 65).await;
    let mut player = Page::open(&h).await;
    player.send("GET", &format!("/api/stream?id={id}&file=0&t=0&video=copy"), &[], "").await;
    assert_eq!(player.head().await.0, 200);
    assert_eq!(player.chunk().await.as_deref(), Some(&b"ftyp"[..]));
    let pid: i32 = std::fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
    let running = || unsafe { libc::kill(pid, 0) == 0 };
    assert!(running());
    // A seek, or a closed tab.
    drop(player);
    let deadline = Instant::now() + Duration::from_secs(10);
    while running() {
        assert!(Instant::now() < deadline, "ffmpeg (pid {pid}) outlived the player's connection");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_ffmpeg_cannot_read_is_an_error_and_not_an_empty_video() {
    let h = Harness::new();
    let complaint = "echo '[matroska,webm @ 0x7a310a4300] EBML header parsing failed' >&2; exit 1";
    StandIns::install(&h, FILM, complaint);
    let id = playing(&h, 66).await;
    let answer = get(&h, &format!("/api/stream?id={id}&file=0&t=0&video=copy")).await;
    assert_eq!(error(&answer), (502, "internal", "ffmpeg cannot convert this file: EBML header parsing failed"));

    // ffmpeg failing after it wrote some video cuts the answer short, without the mark of a whole one.
    StandIns::install(&h, FILM, "printf 'ftyp'; exit 1");
    let mut player = Page::open(&h).await;
    player.send("GET", &format!("/api/stream?id={id}&file=0&t=0&video=copy"), &[], "").await;
    assert_eq!(player.head().await.0, 200);
    assert_eq!(player.chunk().await.as_deref(), Some(&b"ftyp"[..]));
    let mut rest = Vec::new();
    player.0.read_to_end(&mut rest).await.unwrap();
    assert_eq!(rest, b"", "no last chunk");
}

/// When each fragment of each track in a fragmented MP4 starts, in seconds.
fn fragments(mp4: &[u8]) -> Vec<Vec<f64>> {
    let find = |name: &[u8], from: usize| mp4[from..].windows(4).position(|w| w == name).map(|at| at + from);
    let uint = |bytes: &[u8]| bytes.iter().fold(0u64, |n, &b| n << 8 | u64::from(b));
    let mut scales = Vec::new();
    let mut at = 0;
    while let Some(mdhd) = find(b"mdhd", at) {
        let scale = if mp4[mdhd + 4] == 0 { &mp4[mdhd + 16..mdhd + 20] } else { &mp4[mdhd + 24..mdhd + 28] };
        scales.push(uint(scale) as f64);
        at = mdhd + 4;
    }
    let mut starts = vec![Vec::new(); scales.len()];
    let mut at = 0;
    while let Some(tfhd) = find(b"tfhd", at) {
        let track = uint(&mp4[tfhd + 8..tfhd + 12]) as usize - 1;
        let tfdt = find(b"tfdt", tfhd).expect("each fragment says when it starts");
        let time = if mp4[tfdt + 4] == 1 { uint(&mp4[tfdt + 8..tfdt + 16]) } else { uint(&mp4[tfdt + 8..tfdt + 12]) };
        starts[track].push(time as f64 / scales[track]);
        at = tfhd + 4;
    }
    starts
}

/// A 12-second film made with ffmpeg: H.264 with a keyframe each second, silent AC-3, and a
/// subtitle track with a line at 1 and at 9 seconds, and two chapters. `None` when this machine's ffmpeg cannot make it.
fn film(dir: &Path, tools: &Tools) -> Option<Vec<u8>> {
    let srt = "1\n00:00:01,000 --> 00:00:02,000\nfirst\n\n2\n00:00:09,000 --> 00:00:10,000\nninth\n";
    std::fs::write(dir.join("lines.srt"), srt).unwrap();
    let chapter =
        |start: u32, end: u32, title: &str| format!("[CHAPTER]\nTIMEBASE=1/1\nSTART={start}\nEND={end}\n{title}");
    let chapters = [chapter(0, 4, "title=Opening\n"), chapter(4, 12, "")].concat();
    std::fs::write(dir.join("chapters.txt"), format!(";FFMETADATA1\n{chapters}")).unwrap();
    let made = std::process::Command::new(tools.ffmpeg.as_ref()?)
        .current_dir(dir)
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=160x90:rate=24:duration=12"])
        .args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=stereo", "-i", "lines.srt"])
        .args(["-f", "ffmetadata", "-i", "chapters.txt", "-map_chapters", "3", "-t", "12"])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "24"])
        .args(["-c:a", "ac3", "-b:a", "96k", "-c:s", "srt", "-metadata:s:a:0", "language=eng", "made.mkv"])
        .status()
        .is_ok_and(|status| status.success());
    made.then(|| std::fs::read(dir.join("made.mkv")).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_matroska_film_plays_from_any_second_with_its_own_timestamps() {
    let h = Harness::new();
    let tools = Tools::find(std::env::var("PATH").ok().as_deref());
    let Some(made) = tools.both().and_then(|_| film(h.dir.path(), &tools)) else {
        eprintln!("skipped: this machine has no ffmpeg that can make the test film");
        return;
    };
    *h.outside.tools.lock().unwrap() = tools;
    h.swarm.set_rate(100 * GIB);
    // "déjà vu" in Windows-1252, as old subtitle files are.
    let lines = b"1\n00:00:03,000 --> 00:00:04,000\nd\xE9j\xE0 vu\n";
    let film = torrent(67, "film", &[("film.mkv", made.len() as u64), ("film.fr.srt", lines.len() as u64)]);
    std::fs::create_dir_all(h.downloads().join("film")).unwrap();
    std::fs::write(h.downloads().join("film/film.mkv"), &made).unwrap();
    std::fs::write(h.downloads().join("film/film.fr.srt"), lines).unwrap();
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    let (status, playing) = post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    assert_eq!((status, &playing["subtitles"][0]["index"]), (200, &json!(1)), "{playing}");

    let (status, media) = get(&h, &format!("/api/media?id={id}&file=0")).await;
    assert_eq!(status, 200, "{media}");
    assert_eq!(
        json!([
            media["duration"],
            media["container"],
            media["direct"],
            media["remux"],
            media["video"]["codec"],
            media["video"]["width"],
            media["video"]["fps"],
            media["audio"][0]["codec"],
            media["audio"][0]["language"],
            media["subtitles"][0]["codec"],
            media["subtitles"][0]["text"],
        ]),
        json!([12.0, "matroska", null, true, "h264", 160, 24.0, "ac3", "eng", "subrip", true])
    );
    assert!(media["video"]["mime"].as_str().unwrap().starts_with("avc1."), "{media}");
    assert_eq!(
        media["chapters"],
        json!([{ "start": 0.0, "end": 4.0, "title": "Opening" }, { "start": 4.0, "end": 12.0, "title": null }])
    );
    let subtitle = media["subtitles"][0]["stream"].as_u64().unwrap();

    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    let (status, headers, jpeg) = fetch(&h, "GET", &format!("/api/thumbnail?id={id}&file=0&t=10"), &[], "").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&jpeg));
    assert!(headers.contains("content-type: image/jpeg\n"), "{headers}");
    assert!(headers.contains("cache-control: private, max-age=3600\n"), "{headers}");
    assert_eq!(jpeg[..3], [0xff, 0xd8, 0xff], "a JPEG");
    assert_eq!(fetch(&h, "GET", &format!("/api/thumbnail?id={id}&file=0&t=10"), &[], "").await.2, jpeg);
    let past_the_end = get(&h, &format!("/api/thumbnail?id={id}&file=0&t=12")).await;
    assert_eq!(
        error(&past_the_end),
        (404, "not_found", "no picture of film.mkv at 12 s: the video has no frame there")
    );

    let (status, headers, mp4) =
        fetch(&h, "GET", &format!("/api/stream?id={id}&file=0&t=6.5&video=copy"), &[], "").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&mp4));
    assert!(headers.contains("content-type: video/mp4\n"), "{headers}");
    assert_eq!(&mp4[4..8], b"ftyp");
    let [video, audio] = fragments(&mp4).try_into().expect("a video and an audio track");
    assert_eq!(video, [6.0, 7.0, 8.0, 9.0, 10.0, 11.0], "from the keyframe before 6.5 s to the end");
    assert!((audio[0] - 6.5).abs() < 0.1, "audio starts at {} s, not 6.5", audio[0]);

    let text = |path: String| {
        let h = &h;
        async move {
            let (status, _, body) = fetch(h, "GET", &path, &[], "").await;
            (status, String::from_utf8(body).unwrap().replace("\r\n", "\n").trim_end().to_string())
        }
    };
    assert_eq!(
        text(format!("/api/subtitles?id={id}&file=0&stream={subtitle}&t=6")).await,
        (200, "WEBVTT\n\n00:09.000 --> 00:10.000\nninth".to_string())
    );
    assert_eq!(
        text(format!("/api/subtitles?id={id}&file=1")).await,
        (200, "WEBVTT\n\n00:03.000 --> 00:04.000\ndéjà vu".to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_video_without_subtitles_gets_them_from_online_sources() {
    use torrential_core::title::Title;
    use torrential_search::subtitles::FileHash;
    let h = Harness::new();
    let size = 300_000;
    let film = torrent(68, "Sintel (2010) [1080p]", &[("Sintel.2010.1080p.mkv", size), ("poster.jpg", 9_000)]);
    let bytes = written(&h.downloads().join("Sintel (2010) [1080p]/Sintel.2010.1080p.mkv"), size);
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    let online = format!("/api/subtitles/online?id={id}&file=0");

    // The sources are down. That is not an answer to keep.
    h.outside.subtitles.silence(true);
    assert_eq!(get(&h, &online).await, (200, json!({ "title": null, "subtitles": [] })));
    h.outside.subtitles.silence(false);
    let (status, found) = get(&h, &online).await;
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["title"], json!({ "name": "Sintel", "year": 2010, "season": null, "episode": null }));
    let named = |id: &str, language: &str, name: &str| json!({ "id": id, "language": language, "name": name, "exact": false, "source": "fake" });
    assert_eq!(
        found["subtitles"],
        json!([
            named("fake-eng-1", "eng", "Sintel.2010.1080p.en.srt"),
            named("fake-eng-2", "eng", "Another.Release.en.srt"),
            named("fake-spa-1", "spa", "Sintel.2010.1080p.es.srt"),
        ])
    );
    assert_eq!(get(&h, &online).await.1, found);
    let asked = h.outside.subtitles.asked();
    assert_eq!(asked.len(), 2, "the answer is remembered, and the silence was not");
    let sintel = Title { name: "Sintel".into(), year: Some(2010), episode: None };
    assert_eq!((asked[1].titles.first(), asked[1].file_name.as_str()), (Some(&sintel), "Sintel.2010.1080p.mkv"));
    assert_eq!(asked[1].hash, None, "nothing has downloaded, and subtitles do not fetch the file's ends");

    let text = |path: String| {
        let h = &h;
        async move {
            let (status, headers, body) = fetch(h, "GET", &path, &[], "").await;
            (status, headers, String::from_utf8(body).unwrap())
        }
    };
    let (status, headers, vtt) = text(format!("/api/subtitles/online/fetch?id={id}&file=0&subtitle=fake-spa-1")).await;
    assert_eq!(status, 200, "{vtt}");
    assert!(headers.contains("content-type: text/vtt; charset=utf-8\n"), "{headers}");
    assert!(headers.contains("cache-control: private, max-age=3600\n"), "{headers}");
    assert!(
        vtt.starts_with(
            "WEBVTT\n\n00:00:01.000 --> 00:00:03.500\nSubtítulo de prueba a los 00:00:01\n\n\
             00:00:05.000 --> 00:00:07.500\nSubtítulo de prueba a los 00:00:05\n"
        ),
        "{vtt}"
    );
    assert_eq!(text(format!("/api/subtitles/online/fetch?id={id}&file=0&subtitle=fake-spa-1")).await.2, vtt);
    assert_eq!(h.outside.subtitles.fetched(), ["fake-spa-1"], "the converted subtitle is remembered");
    // The page names a subtitle by its ID. It cannot make the engine download an address.
    let refused = [
        get(&h, &format!("/api/subtitles/online/fetch?id={id}&file=0&subtitle=https%3A%2F%2Fexample.org%2Fa.srt"))
            .await,
        get(&h, &format!("/api/subtitles/online/fetch?id={id}&file=0")).await,
        get(&h, &format!("/api/subtitles/online?id={id}&file=1")).await,
    ];
    let refused: Vec<(u16, &str)> = refused.iter().map(error).map(|(status, code, _)| (status, code)).collect();
    assert_eq!(refused, [(404, "not_found"), (400, "usage"), (404, "not_found")]);

    // Once the file is on disk, its hash goes with the search, for subtitles timed for this exact file.
    let whole = Harness::new();
    whole.swarm.set_rate(100 * GIB);
    written(&whole.downloads().join("Sintel (2010) [1080p]/Sintel.2010.1080p.mkv"), size);
    whole.swarm.publish(film.clone());
    post(&whole, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    whole.until("done", |jobs| jobs[0].status.name() == "done").await;
    let (_, found) = get(&whole, &online).await;
    assert_eq!(found["subtitles"][0]["exact"], json!(true));
    let (head, tail) = (&bytes[..65_536], &bytes[bytes.len() - 65_536..]);
    assert_eq!(whole.outside.subtitles.asked()[0].hash, Some(FileHash::of(size, head, tail)));
}

/// What has arrived of file 1 of a download.
async fn pieces(h: &Harness, id: &str) -> String {
    let (status, body) = get(h, &format!("/api/pieces?id={id}&file=1")).await;
    assert_eq!(status, 200, "{body}");
    body["pieces"].as_str().unwrap().to_string()
}

/// The answer to asking for a picture of file 1 of a download at `second`.
async fn thumbnail(h: &Harness, id: &str, second: u32) -> (u16, Value) {
    get(h, &format!("/api/thumbnail?id={id}&file=1&t={second}")).await
}

/// The index of a character of a 200-run piece string as a second of a 5400-second film.
fn second_of(run: usize) -> u32 {
    (run * 5400 / 200) as u32
}

#[tokio::test(flavor = "multi_thread")]
async fn the_player_sees_which_parts_of_a_file_have_arrived_and_gets_pictures_only_of_those() {
    let h = Harness::new();
    let tools = StandIns::install(&h, FILM, "printf 'a jpeg'");
    // A film large enough that the bytes around one second are a small part of it.
    // The fake swarm lands pieces in an order its ID decides. With this one, the pieces withheld
    // below are all in the film's first third, so both of its ends arrive.
    let film = torrent(99, "film", &[("extras.mkv", GIB), ("film.mkv", 20 * GIB)]);
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    post(&h, "/api/play", json!({ "ref": magnet_link(&film), "file": 1 })).await;
    assert_eq!(pieces(&h, &id).await, "0".repeat(200), "nothing has arrived");
    let not_read = thumbnail(&h, &id, 60).await;
    assert_eq!(error(&not_read).2, "no picture of film.mkv at 60 s: its tracks have not been read yet");

    // Some pieces never arrive, so the download stops partly done.
    h.swarm.withhold(film.info_hash.id(), 24);
    h.swarm.set_rate(1000 * GIB);
    h.until("all but the withheld pieces arrived", |jobs| jobs[0].progress.verified > 19 * GIB).await;
    get(&h, &format!("/api/media?id={id}&file=1")).await;
    let map = pieces(&h, &id).await;
    assert_eq!(map.len(), 200);
    assert!(map.bytes().all(|b| (b'0'..=b'8').contains(&b)), "{map}");
    let (arrived, missing): (Vec<usize>, Vec<usize>) = (0..200).partition(|&run| map.as_bytes()[run] == b'8');
    assert!(arrived.len() > 150 && !missing.is_empty(), "{map}");
    assert!(map.starts_with('8') && map.ends_with('8'), "{map}");

    // A picture needs the bytes around its second, with room for a bitrate that varies.
    let whole_around =
        (10..190).find(|&run| map[run - 10..run + 10].bytes().all(|b| b == b'8')).expect("a long whole stretch");
    let (status, headers, jpeg) =
        fetch(&h, "GET", &format!("/api/thumbnail?id={id}&file=1&t={}", second_of(whole_around)), &[], "").await;
    assert_eq!((status, jpeg.as_slice()), (200, &b"a jpeg"[..]), "{map}");
    assert!(headers.contains("content-type: image/jpeg\n"), "{headers}");
    let on_disk = h.downloads().join("film/film.mkv").display().to_string();
    assert_eq!(
        tools.calls().last().unwrap(),
        &format!(
            "ffmpeg -v error -nostdin -ss {}.000 -i file:{on_disk} -frames:v 1 -an -sn -dn -vf scale=240:-2 -q:v 5 \
             -f mjpeg pipe:1",
            second_of(whole_around)
        ),
        "ffmpeg reads the file on disk, which cannot fetch a piece"
    );
    let made = tools.calls().len();
    let in_a_gap = thumbnail(&h, &id, second_of(missing[0])).await;
    assert_eq!(
        error(&in_a_gap),
        (
            404,
            "not_found",
            &*format!("no picture of film.mkv at {} s: that part has not downloaded yet", second_of(missing[0]))
        )
    );
    let again =
        fetch(&h, "GET", &format!("/api/thumbnail?id={id}&file=1&t={}", second_of(whole_around)), &[], "").await;
    assert_eq!(again.2, jpeg);
    assert_eq!(tools.calls().len(), made, "a gap runs nothing, and a picture made once is kept");
    let refused = [
        thumbnail(&h, &id, 5400).await,
        get(&h, &format!("/api/thumbnail?id={id}&file=1")).await,
        get(&h, &format!("/api/pieces?id={id}&file=0")).await,
    ];
    let refused: Vec<(u16, &str)> = refused.iter().map(error).map(|(status, code, _)| (status, code)).collect();
    assert_eq!(refused, [(404, "not_found"), (400, "usage"), (404, "not_found")]);

    // ffmpeg can draw a frame from damaged bytes and only complain. That is no picture.
    StandIns::install(&h, FILM, "printf 'a jpeg'; echo 'error while decoding MB 4 11' >&2");
    let other = (10..190).rev().find(|&run| map[run - 10..run + 10].bytes().all(|b| b == b'8')).unwrap();
    assert_ne!(other, whole_around);
    assert_eq!(thumbnail(&h, &id, second_of(other)).await.0, 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_file_has_every_part_without_asking_the_engine() {
    let h = Harness::new();
    h.swarm.set_rate(100 * GIB);
    let film = torrent(70, "film", &[("film.mkv", 4 * super::MIB)]);
    written(&h.downloads().join("film/film.mkv"), 4 * super::MIB);
    h.swarm.publish(film.clone());
    let id = film.info_hash.id().to_hex();
    post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await;
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    assert_eq!(get(&h, &format!("/api/pieces?id={id}&file=0")).await, (200, json!({ "pieces": "8".repeat(200) })));
}

/// The lines of an NDJSON answer.
fn lines(body: &[u8]) -> Vec<Value> {
    String::from_utf8(body.to_vec()).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

impl Page {
    /// The next line of an NDJSON answer, one to a chunk.
    async fn line(&mut self) -> Option<Value> {
        let chunk = tokio::time::timeout(Duration::from_secs(10), self.chunk()).await.expect("a line arrives")?;
        Some(serde_json::from_slice(&chunk).unwrap())
    }
}

async fn gone_from_the_engine(h: &Harness, ids: &[torrential_core::TorrentId]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut held = Vec::new();
        for id in ids {
            held.push(h.running.ctx.engine.file_progress(*id).await.is_ok());
        }
        if !held.contains(&true) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn write(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"payload").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_plays_from_the_first_of_its_sources_that_starts_and_cleans_up_the_rest() {
    let (h, night) = titles::night();
    let mut page = Page::open(&h).await;
    page.send("POST", "/api/title/play", &["Content-Type: application/json"], r#"{"video":"tt0063350"}"#).await;
    let (status, headers) = page.head().await;
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: application/x-ndjson\n"), "{headers}");
    let mut steps = Vec::new();
    let mut playing = Value::Null;
    while let Some(line) = page.line().await {
        steps.push(titles::step(&line));
        if line["type"] == "playing" {
            playing = line;
        }
    }
    assert_eq!(
        steps,
        [
            "resolved tt0063350 Night of the Living Dead",
            "searched 4",
            "trying 1 of 4 Night.of.the.Living.Dead.1968.720p.BluRay.x264",
            "check file_list",
            r#"skipped a1a1a1 {"reason":"no_file_list","waited_s":1}"#,
            "trying 2 of 4 Night.of.the.Living.Dead.1968.DVD9",
            "check file_list",
            r#"skipped a2a2a2 {"reason":"no_video"}"#,
            "trying 3 of 4 Night.of.the.Living.Dead.1968.1080p.WEB.x264",
            "check file_list",
            "check start",
            "check first_bytes",
            r#"skipped a3a3a3 {"reason":"no_data","waited_s":1}"#,
            "trying 4 of 4 night_of_the_living_dead",
            "check file_list",
            "check start",
            "check first_bytes",
            "playing tt0063350 a4a4a4 file 0 at null",
        ]
    );
    let id = night[3].info_hash.id().to_hex();
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<String>>();
    assert_eq!(keys(&playing), ["play", "release", "resume_at", "type", "video"].map(String::from).into());
    let play_keys: Vec<String> = keys(&playing["play"]).into_iter().collect();
    assert_eq!(
        play_keys,
        ["files", "id", "job", "name", "subtitles", "total_size", "video"],
        "the shape POST /api/play answers"
    );
    assert_eq!(
        json!([playing["release"]["span"], playing["play"]["video"], playing["play"]["subtitles"][0]["path"]]),
        json!([
            "film",
            { "index": 0, "path": "night_of_the_living_dead.mp4", "size": 700 << 20,
              "url": format!("/{id}/0/night_of_the_living_dead.mp4"), "offset": 0 },
            "night_of_the_living_dead.srt"
        ])
    );

    let tried: Vec<_> = night[..3].iter().map(|t| t.info_hash.id()).collect();
    assert!(gone_from_the_engine(&h, &tried).await, "no file list probe or trial is left in the engine");
    let (_, jobs) = get(&h, "/api/jobs").await;
    let listed: Vec<Value> = jobs["jobs"].as_array().unwrap().iter().map(|j| json!([j["id"], j["title"]])).collect();
    assert_eq!(listed, [json!([id, { "title": "tt0063350", "span": "film", "release": id }])]);
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_the_connection_while_a_source_sends_nothing_pauses_its_download_and_keeps_what_it_wrote() {
    let (h, night) = titles::night();
    let third = h.downloads().join("Night.of.the.Living.Dead.1968.1080p.WEB.x264");
    let body = json!({ "video": "tt0063350", "source": night[2].info_hash.id().to_hex() }).to_string();
    let mut page = Page::open(&h).await;
    page.send("POST", "/api/title/play", &["Content-Type: application/json"], &body).await;
    assert_eq!(page.head().await.0, 200);
    while let Some(line) = page.line().await {
        if line == json!({ "type": "check", "check": "first_bytes" }) {
            break;
        }
    }
    write(&third.join("Night.1968.1080p.mp4"));
    assert_eq!(h.status().await.len(), 1, "the trial's download runs");
    drop(page);
    h.until("the trial is paused", |jobs| jobs[0].status.name() == "paused").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.states().await, ["paused"]);
    assert_eq!(std::fs::read(third.join("Night.1968.1080p.mp4")).unwrap(), b"payload");
}

#[tokio::test(flavor = "multi_thread")]
async fn where_a_person_stopped_shows_on_every_connection_and_decides_what_comes_next() {
    let h = Harness::new();
    let watch = |video: &str, position: f64, duration: f64| json!({ "video": video, "position": position, "duration": duration });
    let (status, saved) = post(&h, "/api/watched", watch("tt9999901:1:1", 2630.0, 2640.0)).await;
    assert_eq!(status, 200);
    let entry = &saved["watched"];
    assert_eq!(
        json!([entry["video"], entry["position"], entry["duration"], entry["finished"], entry["at"].is_i64()]),
        json!(["tt9999901:1:1", 2630.0, 2640.0, true, true])
    );
    post(&h, "/api/watched", watch("tt0063350", 1834.2, 5760.0)).await;

    let undertow = json!({ "season": 1, "episode": 2, "name": "Undertow", "aired": 1_557_273_600, "overview": null, "thumbnail": true });
    let harbor = json!({ "id": "tt9999901", "kind": "series", "name": "Harbor Lights", "year": 2019, "rating": 8.1 });
    let night =
        json!({ "id": "tt0063350", "kind": "film", "name": "Night of the Living Dead", "year": 1968, "rating": 7.8 });
    let (_, home) = get(&h, "/api/home").await;
    assert_eq!(
        home,
        json!({
            "continue": [
                { "title": night, "video": "tt0063350", "episode": null, "resume_at": 1834.2, "duration": 5760.0 },
                { "title": harbor, "video": "tt9999901:1:2", "episode": undertow, "resume_at": null, "duration": null },
            ],
            "saved": [],
        })
    );
    let (_, page) = get(&h, "/api/title?id=tt9999901").await;
    assert_eq!(
        json!([
            page["name"],
            page["saved"],
            page["watched"][0]["video"],
            page["watched"][0]["finished"],
            page["next"],
            page["downloads"]
        ]),
        json!(["Harbor Lights", false, "tt9999901:1:1", true, { "video": "tt9999901:1:2", "resume_at": null }, []])
    );

    post(&h, "/api/watched", watch("tt9999901:2:1", 2639.0, 2640.0)).await;
    let (_, page) = get(&h, "/api/title?id=tt9999901").await;
    assert_eq!(page["next"], Value::Null, "2x2 has not aired, so nothing comes after 2x1");
    post(&h, "/api/watched", watch("tt0063350", 5750.0, 5760.0)).await;
    assert_eq!(
        get(&h, "/api/home").await.1["continue"],
        json!([]),
        "a finished film and a caught up series are left out"
    );

    assert_eq!(
        post(&h, "/api/library", json!({ "title": "tt0063350", "saved": true })).await,
        (200, json!({ "saved": true }))
    );
    assert_eq!(get(&h, "/api/home").await.1["saved"], json!([night]));
    assert_eq!(get(&h, "/api/title?id=tt0063350").await.1["saved"], json!(true));
    assert_eq!(
        post(&h, "/api/library", json!({ "title": "tt0063350", "saved": false })).await,
        (200, json!({ "saved": false }))
    );
    assert_eq!(get(&h, "/api/home").await.1["saved"], json!([]));

    let refused = [
        post(&h, "/api/watched", json!({ "video": "tt0063350", "position": -1.0, "duration": null })).await,
        post(&h, "/api/watched", json!({ "video": "night", "position": 1.0, "duration": null })).await,
    ];
    assert_eq!(refused.iter().map(|r| error(r).0).collect::<Vec<_>>(), [400, 400]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_position_or_duration_no_video_can_have_is_refused_and_nothing_is_saved() {
    let h = Harness::new();
    let watch = |position: f64, duration: Value| {
        let body = json!({ "video": "tt0063350", "position": position, "duration": duration });
        let h = &h;
        async move {
            let answer = post(h, "/api/watched", body).await;
            let (status, code, message) = error(&answer);
            (status, code.to_string(), message.to_string())
        }
    };
    let usage = |message: &str| (400, "usage".to_string(), message.to_string());
    let refused = [
        watch(-1.0, json!(null)).await,
        watch(3_600_001.0, json!(null)).await,
        watch(1e30, json!(null)).await,
        watch(60.0, json!(-5.0)).await,
        watch(60.0, json!(0.0)).await,
        watch(60.0, json!(3_600_001.0)).await,
        watch(60.0, json!(1e30)).await,
        watch(700.5, json!(600.0)).await,
    ];
    assert_eq!(
        refused,
        [
            usage("position -1 is not a time in a video"),
            usage("position 3600001 is not a time in a video"),
            usage("position 1000000000000000000000000000000 is not a time in a video"),
            usage("duration -5 is not the length of a video"),
            usage("duration 0 is not the length of a video"),
            usage("duration 3600001 is not the length of a video"),
            usage("duration 1000000000000000000000000000000 is not the length of a video"),
            usage("position 700.5 is past the end of a video 600 seconds long"),
        ]
    );
    assert_eq!(get(&h, "/api/title?id=tt0063350").await.1["watched"], json!([]));

    assert_eq!(watch(3_600_000.0, json!(3_600_000.0)).await.0, 200, "1,000 hours is the longest");
    assert_eq!(watch(600.0, json!(600.0)).await.0, 200);
    assert_eq!(watch(0.0, json!(null)).await.0, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_position_is_saved_only_for_a_film_or_an_aired_episode_the_catalog_lists() {
    let h = Harness::new();
    let mut answers = Vec::new();
    for video in ["tt9999901:2:2", "tt9999901:2:3", "tt9999901:9:1", "tt9999901", "tt0063350:1:1"] {
        let answer = post(&h, "/api/watched", json!({ "video": video, "position": 300.0, "duration": 2640.0 })).await;
        let (status, code, message) = error(&answer);
        answers.push((status, code.to_string(), message.to_string()));
    }
    let usage = |message: &str| (400, "usage".to_string(), message.to_string());
    assert_eq!(
        answers,
        [
            usage("Harbor Lights S02E02 has not aired yet"),
            usage("Harbor Lights S02E03 has not aired yet"),
            usage("S09E01 is not an episode of Harbor Lights"),
            usage("Harbor Lights is a series, so name one of its episodes"),
            usage("Night of the Living Dead is a film, so it has no S01E01"),
        ]
    );
    let (_, page) = get(&h, "/api/title?id=tt9999901").await;
    assert_eq!(
        (&page["watched"], &page["next"]),
        (&json!([]), &json!({ "video": "tt9999901:1:1", "resume_at": null }))
    );
    assert_eq!(post(&h, "/api/watched", json!({ "video": "tt9999901:2:1", "position": 300.0 })).await.0, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn titles_their_pictures_and_their_sources_come_from_the_catalog_and_the_search() {
    let (h, _) = titles::night();
    let (_, found) = get(&h, "/api/titles?q=night%20of%20the%20living%20dead").await;
    let cards: Vec<Value> = found["titles"].as_array().unwrap().iter().map(|c| json!([c["id"], c["year"]])).collect();
    assert_eq!(cards, [json!(["tt0063350", 1968]), json!(["tt0100258", 1990])]);
    assert_eq!(get(&h, "/api/titles?q=zzzz").await, (200, json!({ "titles": [] })), "nothing matches");

    let (status, headers, poster) = fetch(&h, "GET", "/api/title/art?id=tt0063350&kind=poster", &[], "").await;
    assert_eq!((status, poster.len()), (200, 67));
    assert!(
        headers.contains("content-type: image/png\n") && headers.contains("cache-control: private, max-age=2592000\n"),
        "{headers}"
    );
    assert_eq!(fetch(&h, "GET", "/api/title/art?id=tt9999901&kind=thumbnail&season=1&episode=1", &[], "").await.0, 200);
    let (status, _, missing) = fetch(&h, "GET", "/api/title/art?id=tt0100258&kind=poster", &[], "").await;
    let missing: Value = serde_json::from_slice(&missing).unwrap();
    assert_eq!((status, &missing["error"]["code"]), (404, &json!("not_found")));
    assert_eq!(error(&get(&h, "/api/title/art?id=tt0063350&kind=banner").await).0, 400);

    let phone = "/api/title/sources?video=tt0063350&width=1170&height=2532&scale=3&playback=hevc";
    let (status, headers, body) = fetch_from(&h, "192.168.1.20", "GET", phone, &[], "").await;
    assert_eq!(status, 200);
    assert!(headers.contains("content-type: application/x-ndjson\n"), "{headers}");
    let done = lines(&body).pop().unwrap();
    let sources: Vec<Value> = done["result"]["releases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!([r["name"], r["span"], r["swarm"]]))
        .collect();
    assert_eq!(
        sources,
        [
            json!(["Night.of.the.Living.Dead.1968.720p.BluRay.x264", "film", "strong"]),
            json!(["Night.of.the.Living.Dead.1968.DVD9", "film", "strong"]),
            json!(["Night.of.the.Living.Dead.1968.1080p.WEB.x264", "film", "strong"]),
            json!(["night_of_the_living_dead", "film", "strong"]),
        ]
    );
    let profile = &done["result"]["query"]["profile"];
    assert_eq!(
        (&profile["screen"], &profile["playback"], &profile["preferences"]["playback"]),
        (
            &json!({ "value": "720p", "source": "viewer" }),
            &json!({ "value": "hevc", "source": "viewer" }),
            &Value::Null
        )
    );
    assert_eq!(error(&get(&h, "/api/title/sources?video=tt9999901:2:2").await).0, 400, "it has not aired");

    let (_, _, raw) = fetch(&h, "GET", "/api/search?q=night", &[], "").await;
    let raw = lines(&raw).pop().unwrap();
    let raw = &raw["result"]["releases"][0];
    assert!(raw.get("span").is_none() && raw.get("swarm").is_none(), "a torrent search names no span or swarm");
    assert_eq!(
        error(&post(&h, "/api/play", json!({ "ref": "tt0063350" })).await),
        (404, "invalid_ref", "tt0063350 is a film or a series, not a torrent")
    );
    let (_, refused) = post(&h, "/api/play", json!({ "ref": "tt0063350" })).await;
    assert_eq!(refused["error"]["hint"], json!("play a film or a series with POST /api/title/play"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_ffmpeg_the_browser_passes_over_a_matroska_film_that_a_media_player_plays() {
    use torrential_core::protocol::{Request, Response, TitleRequest};
    let film = torrent(0xb1, "Night.of.the.Living.Dead.1968.1080p.BluRay.x264", &[("Night.1968.1080p.mkv", 2 * GIB)]);
    let catalog = vec![titles::listed(&film, "archive.org", None)];
    let script = vec![FakeSource::answers("archive.org", 0)];
    let h = Harness::with_search(search_port::Fake::scripted(catalog, script, Default::default()));
    h.swarm.publish(film.clone());
    h.swarm.set_rate(GIB);
    let ask = r#"{"video":"tt0063350"}"#;
    let (status, _, body) = fetch(&h, "POST", "/api/title/play", &["Content-Type: application/json"], ask).await;
    let steps: Vec<String> = lines(&body).iter().map(titles::step).collect();
    let skipped = r#"b1b1b1 {"reason":"unplayable","container":"mkv"}"#;
    assert_eq!(
        (status, steps),
        (
            200,
            vec![
                "resolved tt0063350 Night of the Living Dead".to_string(),
                "searched 1".into(),
                "trying 1 of 1 Night.of.the.Living.Dead.1968.1080p.BluRay.x264".into(),
                "check file_list".into(),
                format!("skipped {skipped}"),
                format!(
                    "failed no_source: no source of Night of the Living Dead (1968) plays in the browser without \
                     ffmpeg {:?}",
                    [skipped]
                ),
            ]
        )
    );

    let video = "tt0063350".parse().unwrap();
    let Response::PlayTitle { reply, title } = h.ask(Request::PlayTitle(TitleRequest { video, source: None })).await
    else {
        panic!("the command line's play answers a title")
    };
    let streams: Vec<&str> = reply.streams.iter().map(|s| s.path.as_str()).collect();
    assert_eq!((reply.job.id, streams, title.tried), (film.info_hash.id(), vec!["Night.1968.1080p.mkv"], vec![]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_screen_measured_in_part_or_without_pixels_is_refused() {
    let (h, _) = titles::night();
    assert_eq!(
        error(&get(&h, "/api/title/sources?video=tt0063350&width=1170&playback=hevc").await),
        (400, "usage", "a viewer needs all of width, height, scale, and playback")
    );
    let flat = json!({ "width": 1170, "height": 2532, "scale": 0, "playback": "hevc" });
    assert_eq!(
        error(&post(&h, "/api/title/play", json!({ "video": "tt0063350", "viewer": flat })).await),
        (400, "usage", "scale 0 is not a number of device pixels to a CSS pixel")
    );
    let dark = json!({ "width": 0, "height": 2160, "scale": 1, "playback": "hevc" });
    assert_eq!(
        error(&post(&h, "/api/title/play", json!({ "video": "tt0063350", "viewer": dark })).await),
        (400, "usage", "a 0x2160 screen has no pixels")
    );
}

/// The releases `GET /api/title/sources` lists for `video`, each with why it failed lately.
async fn listed_sources(h: &Harness, video: &str) -> Vec<(String, Value)> {
    let (status, _, body) = fetch(h, "GET", &format!("/api/title/sources?video={video}"), &[], "").await;
    let done = lines(&body).pop().unwrap();
    assert_eq!((status, &done["type"]), (200, &json!("done")));
    let releases = done["result"]["releases"].as_array().unwrap().iter();
    releases.map(|r| (r["name"].as_str().unwrap().to_string(), r["failed"].clone())).collect()
}

/// The releases a title play over HTTP tried, in order.
async fn tried_by_play(h: &Harness, video: &str) -> Vec<String> {
    let ask = json!({ "video": video }).to_string();
    let (_, _, body) = fetch(h, "POST", "/api/title/play", &["Content-Type: application/json"], &ask).await;
    let trying = lines(&body).into_iter().filter(|line| line["type"] == "trying");
    trying.map(|line| line["release"]["name"].as_str().unwrap().to_string()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_tries_a_titles_sources_in_the_order_listed_and_the_list_then_puts_those_that_failed_last_with_why() {
    use torrential_core::protocol::{Request, Response};
    let (h, night) = titles::night();
    let name = |n: usize| night[n].name.clone();
    let fresh: Vec<(String, Value)> = (0..4).map(|n| (name(n), Value::Null)).collect();
    assert_eq!(listed_sources(&h, "tt0063350").await, fresh);
    assert_eq!(tried_by_play(&h, "tt0063350").await, (0..4).map(name).collect::<Vec<_>>());

    let after = vec![
        (name(3), Value::Null),
        (name(0), json!({ "reason": "no_file_list", "waited_s": 1 })),
        (name(1), json!({ "reason": "no_video" })),
        (name(2), json!({ "reason": "no_data", "waited_s": 1 })),
    ];
    assert_eq!(listed_sources(&h, "tt0063350").await, after);
    let video = "tt0063350".parse().unwrap();
    let Response::Sources { sources } = h.ask(Request::Sources { video }).await else {
        panic!("the socket answers sources with the releases")
    };
    let socket = sources.result.releases.iter().map(|r| (r.name.clone(), serde_json::to_value(&r.failed).unwrap()));
    assert_eq!(socket.collect::<Vec<_>>(), after, "the command line lists the same order");
    assert_eq!(tried_by_play(&h, "tt0063350").await, [name(3)], "the download that played plays again first");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_web_app_and_the_socket_read_the_same_title_page() {
    use torrential_core::protocol::{Request, Response};
    let h = Harness::new();
    post(&h, "/api/watched", json!({ "video": "tt0063350", "position": 60.0, "duration": 5760.0 })).await;
    let (status, page) = get(&h, "/api/title?id=tt0063350").await;
    let Response::Title { title } = h.ask(Request::Title { id: "tt0063350".parse().unwrap() }).await else {
        panic!("the socket answers a title with its page")
    };
    assert_eq!((status, &page), (200, &serde_json::to_value(&title).unwrap()));
    let mut page = page;
    let at = page["watched"][0].as_object_mut().unwrap().remove("at");
    assert!(at.is_some_and(|at| at.is_i64()));
    assert_eq!(
        page,
        json!({
            "id": "tt0063350",
            "kind": "film",
            "name": "Night of the Living Dead",
            "year": 1968,
            "rating": 7.8,
            "synopsis": "Strangers shelter in a farmhouse while the dead walk outside.",
            "genres": ["Horror", "Thriller"],
            "runtime_min": 96,
            "cast": ["Duane Jones", "Judith O'Dea", "Karl Hardman"],
            "countries": ["US"],
            "art": { "poster": true, "background": true, "logo": true },
            "seasons": [],
            "saved": false,
            "watched": [{ "video": "tt0063350", "position": 60.0, "duration": 5760.0, "finished": false }],
            "next": { "video": "tt0063350", "resume_at": 60.0 },
            "downloads": []
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn with_title_lookups_off_title_search_says_so_and_torrent_search_still_works() {
    use torrential_search::catalog::CatalogConfig;
    let off = CatalogConfig { online: false, endpoint: None, imdb_endpoint: None, cache_dir: None };
    let catalog = std::sync::Arc::new(super::super::titles::catalog_port::Live::new(&off).unwrap());
    let h = Harness::with_titles(catalog);
    assert_eq!(get(&h, "/api/titles?q=night").await, (200, json!({ "titles": [], "offline": true })));
    assert_eq!(
        error(&get(&h, "/api/title?id=tt0063350").await),
        (400, "usage", "title lookups are off in config.yaml, and tt0063350 is not kept on this machine")
    );
    assert_eq!(fetch(&h, "GET", "/api/title/art?id=tt0063350&kind=poster", &[], "").await.0, 404);
    let film = torrent(64, "film", &[("film.mp4", GIB)]);
    h.swarm.publish(film.clone());
    assert_eq!(post(&h, "/api/play", json!({ "ref": magnet_link(&film) })).await.0, 200);
    let (_, jobs) = get(&h, "/api/jobs").await;
    assert!(jobs["jobs"][0].get("title").is_none(), "a download made by playing a torrent names no title");
    let (status, _, body) = fetch(&h, "GET", "/api/search?q=ubuntu", &[], "").await;
    let done = lines(&body).pop().unwrap();
    assert_eq!(
        (status, &done["type"], &done["result"]["releases"][0]["name"]),
        (200, &json!("done"), &json!("ubuntu-24.04.5-desktop-amd64.iso"))
    );
}

/// A MacBook's panel. In its More Space mode the desktop lays out 1800x1169 at 2x, so a browser
/// measures a frame of 3600x2338, larger than the panel.
const PANEL: Display = Display { width: 3024, height: 1964, hdr: true };
/// An iPhone's screen, as the web app measures it.
const PHONE: &str = "width=1170&height=2532&scale=3&playback=hevc";

/// What the sources listed at `path` for a browser at `from` were ranked for: the screen and the
/// playback, each with where it came from.
async fn listed_for(h: &Harness, from: &str, path: &str) -> (Value, Value) {
    let (status, _, body) = fetch_from(h, from, "GET", path, &[], "").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let profile = lines(&body).pop().unwrap()["result"]["query"]["profile"].clone();
    (profile["screen"].clone(), profile["playback"].clone())
}

/// What the last search of a play was ranked for, as `listed_for` reads it.
fn played_for(h: &Harness) -> (Value, Value) {
    let profile = serde_json::to_value(h.search.ranked_for().expect("the play searched")).unwrap();
    (profile["screen"].clone(), profile["playback"].clone())
}

async fn play_from(h: &Harness, from: &str, ask: Value) -> Vec<Value> {
    let headers = ["Content-Type: application/json"];
    let (status, _, body) = fetch_from(h, from, "POST", "/api/title/play", &headers, &ask.to_string()).await;
    let lines = lines(&body);
    assert_eq!((status, &lines.last().unwrap()["type"]), (200, &json!("playing")), "{lines:#?}");
    lines
}

#[tokio::test(flavor = "multi_thread")]
async fn this_mac_ranks_for_its_own_panel_and_its_browser_for_what_it_decodes_whatever_config_yaml_says() {
    let hevc_hdr = Preferences { playback: Some(Playback::HevcHdr), ..Preferences::default() };
    let (h, _) = titles::night_with(Outside { display: Some(PANEL), preferences: hevc_hdr, ..Outside::default() });
    let scaled = "/api/title/sources?video=tt0063350&width=3600&height=2338&scale=2&playback=h264";
    let mac = (json!({ "value": "1080p", "source": "detected" }), json!({ "value": "h264", "source": "viewer" }));
    assert_eq!(listed_for(&h, "127.0.0.1", scaled).await, mac);
    let viewer = json!({ "width": 3600, "height": 2338, "scale": 2, "playback": "h264" });
    play_from(&h, "127.0.0.1", json!({ "video": "tt0063350", "viewer": viewer })).await;
    assert_eq!(played_for(&h), mac);
}

#[tokio::test(flavor = "multi_thread")]
async fn another_device_ranks_for_the_screen_it_reports_and_config_yaml_only_for_this_mac() {
    let tv = Preferences { screen: Some(Screen::Tv4k), playback: Some(Playback::H264), ..Preferences::default() };
    let (h, _) = titles::night_with(Outside { display: Some(PANEL), preferences: tv, ..Outside::default() });
    let phone = format!("/api/title/sources?video=tt0063350&{PHONE}");
    let on_phone = (json!({ "value": "720p", "source": "viewer" }), json!({ "value": "hevc", "source": "viewer" }));
    assert_eq!(listed_for(&h, "192.168.1.20", &phone).await, on_phone);
    assert_eq!(
        listed_for(&h, "127.0.0.1", &phone).await,
        (json!({ "value": "2160p", "source": "override" }), json!({ "value": "hevc", "source": "viewer" })),
        "the same numbers from this Mac rank for the screen in config.yaml"
    );
    let viewer = json!({ "width": 1170, "height": 2532, "scale": 3, "playback": "hevc" });
    play_from(&h, "192.168.1.20", json!({ "video": "tt0063350", "viewer": viewer })).await;
    assert_eq!(played_for(&h), on_phone);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tab_opened_before_an_update_ranks_for_what_it_decodes_on_this_macs_screen() {
    let hevc = Preferences { playback: Some(Playback::Hevc), ..Preferences::default() };
    let (h, _) = titles::night_with(Outside { display: Some(PANEL), preferences: hevc, ..Outside::default() });
    let mac = (json!({ "value": "1080p", "source": "detected" }), json!({ "value": "h264", "source": "viewer" }));
    assert_eq!(listed_for(&h, "127.0.0.1", "/api/title/sources?video=tt0063350&playback=h264").await, mac);
    play_from(&h, "127.0.0.1", json!({ "video": "tt0063350", "playback": "h264" })).await;
    assert_eq!(played_for(&h), mac);
}

/// The name of the source a play from `from` on `viewer` tries first.
async fn first_tried(h: &Harness, from: &str, viewer: &Value) -> String {
    let lines = play_from(h, from, json!({ "video": "tt0063350", "viewer": viewer })).await;
    let trying = lines.iter().find(|line| line["type"] == "trying").unwrap();
    trying["release"]["name"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_here_plays_first_only_where_the_browser_decodes_it_or_ffmpeg_converts_it() {
    let (h, night) = titles::night();
    let hdr = torrent(0xb1, "Night.of.the.Living.Dead.1968.2160p.UHD.BluRay.REMUX.HDR.HEVC", &[("Night.mp4", MIB)]);
    let remux = torrent(0xb2, "Night.of.the.Living.Dead.1968.1080p.BluRay.REMUX.AVC", &[("Night.mp4", MIB)]);
    for t in [&hdr, &remux] {
        h.swarm.publish(t.clone());
        h.play_for(t, &[0], Some("tt0063350")).await.unwrap();
    }
    let phone = json!({ "width": 1170, "height": 2532, "scale": 3, "playback": "h264" });
    let tv = json!({ "width": 3840, "height": 2160, "scale": 1, "playback": "hevc-hdr" });

    let (status, _, body) = fetch_from(
        &h,
        "192.168.1.20",
        "GET",
        "/api/title/sources?video=tt0063350&width=1170&height=2532&scale=3&playback=h264",
        &[],
        "",
    )
    .await;
    assert_eq!(status, 200);
    let done = lines(&body).pop().unwrap();
    let listed: Vec<&str> =
        done["result"]["releases"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    let catalog: Vec<&str> = night.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(listed, [[remux.name.as_str()].as_slice(), &catalog, &[hdr.name.as_str()]].concat());

    assert_eq!(first_tried(&h, "192.168.1.20", &phone).await, remux.name, "a remux here costs no download");
    assert_eq!(first_tried(&h, "192.168.1.30", &tv).await, hdr.name);
    StandIns::install(&h, FILM, "");
    assert_eq!(first_tried(&h, "192.168.1.20", &phone).await, hdr.name, "ffmpeg converts it for the phone");
}

/// A Torznab indexer on this machine that answers every search with `releases`, each a torrent
/// with the size and seeders it claims.
async fn indexer(releases: &[(&FakeTorrentFile, u64, u32)]) -> String {
    let items: String = releases
        .iter()
        .map(|(t, size, seeders)| {
            format!(
                "<item><title>{}</title><size>{size}</size><category>2000</category>\
                 <torznab:attr name=\"seeders\" value=\"{seeders}\" />\
                 <torznab:attr name=\"infohash\" value=\"{}\" /></item>",
                t.name,
                t.info_hash.id().to_hex()
            )
        })
        .collect();
    let feed = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <rss version=\"2.0\" xmlns:torznab=\"http://torznab.com/schemas/2015/feed\"><channel>{items}</channel></rss>"
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let feed = feed.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let answer = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{feed}",
                    feed.len()
                );
                let _ = socket.write_all(answer.as_bytes()).await;
            });
        }
    });
    url
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_and_a_tv_on_the_home_network_each_get_the_copy_for_their_screen() {
    let uhd = torrent(0xc1, "Night of the Living Dead 1968 2160p UHD BluRay x265 HDR", &[("Night.mp4", MIB)]);
    let hd = torrent(0xc2, "Night of the Living Dead 1968 1080p BluRay x264", &[("Night.mp4", MIB)]);
    let sd = torrent(0xc3, "Night Of The Living Dead 1968 720p BRRip x264", &[("Night.mp4", MIB)]);
    let url = indexer(&[(&uhd, 12 * GIB, 50), (&hd, 4 * GIB, 232), (&sd, 1700 * MIB, 22)]).await;
    let builtin = ["archlinux", "ubuntu", "debian", "archive.org", "academictorrents", "knaben", "apibay", "nyaa"];
    let config = torrential_search::SearchConfig {
        cache_dir: None,
        torznab: vec![torrential_search::TorznabConfig { name: "home".into(), url, api_key: None }],
        endpoints: std::collections::BTreeMap::new(),
        disabled: builtin.map(String::from).to_vec(),
    };
    let search: Arc<dyn SearchPort> = Arc::new(search_port::Live::new(&config).unwrap());
    let phone_screen = Preferences { screen: Some(Screen::Phone), playback: Some(Playback::H264), max_size: None };
    let outside = Outside { preferences: phone_screen, search: Some(search), ..Outside::default() };
    let h = Harness::with_outside(search_port::Fake::default(), outside);
    h.play_for(&uhd, &[0], Some("tt0063350")).await.unwrap();

    let listed = |from: &'static str, viewer: &'static str| {
        let h = &h;
        async move {
            let path = format!("/api/title/sources?video=tt0063350&{viewer}");
            let (status, _, body) = fetch_from(h, from, "GET", &path, &[], "").await;
            assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
            let done = lines(&body).pop().unwrap();
            let names = done["result"]["releases"].as_array().unwrap().iter();
            names.map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        }
    };
    assert_eq!(
        listed("192.168.1.20", "width=1170&height=2532&scale=3&playback=h264").await,
        [sd.name.as_str(), &hd.name, &uhd.name],
        "the 4K copy here neither plays on the phone nor fits it"
    );
    assert_eq!(
        listed("192.168.1.30", "width=3840&height=2160&scale=1&playback=hevc-hdr").await,
        [uhd.name.as_str(), &hd.name, &sd.name],
        "phone and h264 in config.yaml are for this Mac"
    );
}
