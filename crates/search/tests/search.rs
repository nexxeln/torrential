use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrential_core::error::Details;
use torrential_core::title::{Namesakes, Wanted};
use torrential_core::{Category, ErrorCode, Flag, HexPrefix, SearchQuery, SourceOutcome};
use torrential_search::{Progress, ResultCache, SavedLink, Search, SearchConfig, TorznabConfig};

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    delay: Duration,
    stall: Duration,
    content_length: bool,
}

fn ok(body: Vec<u8>) -> Reply {
    Reply { status: 200, body, delay: Duration::ZERO, stall: Duration::ZERO, content_length: true }
}

fn status(code: u16) -> Reply {
    Reply { status: code, body: b"no".to_vec(), delay: Duration::ZERO, stall: Duration::ZERO, content_length: true }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)).unwrap()
}

struct Server {
    base: String,
    hits: Arc<Mutex<HashMap<&'static str, usize>>>,
}

impl Server {
    async fn start(routes: Vec<(&'static str, Vec<Reply>)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits: Arc<Mutex<HashMap<&'static str, usize>>> = Arc::default();
        let (routes, counter) = (Arc::new(routes), hits.clone());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (routes, counter) = (routes.clone(), counter.clone());
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut buf = [0u8; 4096];
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string();
                    let reply = match routes.iter().find(|(prefix, _)| path.starts_with(prefix)) {
                        Some((prefix, replies)) => {
                            let mut counter = counter.lock().unwrap();
                            let n = counter.entry(*prefix).or_insert(0);
                            *n += 1;
                            replies[(*n - 1).min(replies.len() - 1)].clone()
                        }
                        None => status(404),
                    };
                    tokio::time::sleep(reply.delay).await;
                    let mut response = format!("HTTP/1.1 {} X\r\nConnection: close\r\n", reply.status);
                    if reply.content_length {
                        response.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
                    }
                    response.push_str("\r\n");
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                    tokio::time::sleep(reply.stall).await;
                    let _ = socket.write_all(&reply.body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Server { base, hits }
    }

    fn hits(&self, prefix: &str) -> usize {
        self.hits.lock().unwrap().get(prefix).copied().unwrap_or(0)
    }

    fn config(&self, sources: &[&str]) -> SearchConfig {
        let endpoints: BTreeMap<String, String> =
            sources.iter().map(|s| (s.to_string(), format!("{}/{s}/", self.base))).collect();
        SearchConfig { endpoints, ..SearchConfig::default() }
    }
}

fn query(text: &str, sources: &[&str]) -> SearchQuery {
    SearchQuery { sources: sources.iter().map(|s| s.to_string()).collect(), timeout_ms: 3000, ..SearchQuery::new(text) }
}

fn outcomes(result: &torrential_core::SearchResult) -> Vec<(String, SourceOutcome)> {
    result.sources.iter().map(|r| (r.source.clone(), r.outcome.clone())).collect()
}

#[tokio::test]
async fn merges_two_indexes_into_one_ranked_list() {
    let server = Server::start(vec![
        ("/knaben/v2/search", vec![ok(fixture("knaben/v2-search-ubuntu.json"))]),
        ("/apibay/q.php", vec![ok(fixture("apibay/q.php-ubuntu.json"))]),
    ])
    .await;
    let search = Search::new(&server.config(&["knaben", "apibay"]), ResultCache::new()).unwrap();
    let mut q = query("ubuntu", &["knaben", "apibay"]);
    q.limit = 200;

    let result = search.search(&q).await.unwrap();

    assert_eq!(
        outcomes(&result),
        [("knaben".into(), SourceOutcome::Ok { results: 98 }), ("apibay".into(), SourceOutcome::Ok { results: 100 })]
    );
    assert_eq!(result.total, 158);
    let lts = result.releases.iter().find(|r| r.name == "Ubuntu 22.04 LTS").unwrap();
    assert_eq!(lts.id.to_ref(), "2c6b6858d61da9543d4231a71db4b1c9264b0685");
    assert_eq!(lts.sources, ["knaben", "apibay"]);
    assert_eq!((lts.seeders, lts.leechers), (Some(37), Some(5)));
    assert_eq!(lts.size, Some(3_654_957_056));
    assert_eq!(lts.flags.iter().collect::<Vec<_>>(), [&Flag::Trusted]);
    let escaped = result.releases.iter().find(|r| r.id.to_ref().starts_with("c426b415")).unwrap();
    assert_eq!(escaped.name, "Linux Command Line (Hands-On) Ubuntu Linux & Linux Scripting");
    assert_eq!(escaped.size, Some(2_464_274_774));
    assert_eq!((escaped.seeders, escaped.leechers), (Some(13), Some(3)));
}

#[tokio::test]
async fn every_source_failing_is_providers_unavailable() {
    let refused = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let server = Server::start(vec![
        ("/knaben/", vec![status(503)]),
        ("/archive.org/", vec![ok(b"<html>Just a moment...</html>".to_vec())]),
    ])
    .await;
    let mut config = server.config(&["knaben", "archive.org"]);
    config.endpoints.insert("apibay".into(), format!("http://{refused}/"));
    let search = Search::new(&config, ResultCache::new()).unwrap();

    let err = search.search(&query("ubuntu", &["knaben", "apibay", "archive.org"])).await.unwrap_err();

    assert_eq!(err.code, ErrorCode::ProvidersUnavailable);
    assert_eq!(err.message, "all sources failed (archive.org, knaben, apibay)");
    let Some(Details::Sources(reports)) = err.details else { panic!("no source reports") };
    let kinds: Vec<(&str, &str)> = reports
        .iter()
        .map(|r| {
            let kind = match &r.outcome {
                SourceOutcome::Http { status: 503 } => "http 503",
                SourceOutcome::Connect { .. } => "connect",
                SourceOutcome::BadResponse { .. } => "bad_response",
                _ => "other",
            };
            (r.source.as_str(), kind)
        })
        .collect();
    assert_eq!(kinds, [("archive.org", "bad_response"), ("knaben", "http 503"), ("apibay", "connect")]);
    assert_eq!(server.hits("/knaben/"), 2);
}

#[tokio::test]
async fn failure_reasons_never_carry_terminal_escapes() {
    let hostile =
        b"<?xml version=\"1.0\"?><error code=\"1\" description=\"\x1b]52;c;cm0gLXJm\x07\x1b[2Jgone\"/>".to_vec();
    let server =
        Server::start(vec![("/nyaa/", vec![ok(hostile)]), ("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])])
            .await;
    let search = Search::new(&server.config(&["nyaa", "apibay"]), ResultCache::new()).unwrap();

    let result = search.search(&query("ubuntu", &["nyaa", "apibay"])).await.unwrap();

    assert_eq!(
        outcomes(&result)[1],
        ("nyaa".into(), SourceOutcome::BadResponse { reason: "indexer error 1: gone".into() })
    );
}

#[tokio::test]
async fn retries_once_after_a_5xx_but_not_after_a_4xx() {
    let server = Server::start(vec![
        ("/apibay/", vec![status(502), ok(fixture("apibay/q.php-ubuntu.json"))]),
        ("/knaben/", vec![status(404)]),
    ])
    .await;
    let search = Search::new(&server.config(&["knaben", "apibay"]), ResultCache::new()).unwrap();

    let result = search.search(&query("ubuntu", &["knaben", "apibay"])).await.unwrap();

    assert_eq!(
        outcomes(&result),
        [("knaben".into(), SourceOutcome::Http { status: 404 }), ("apibay".into(), SourceOutcome::Ok { results: 100 })]
    );
    assert_eq!((server.hits("/apibay/"), server.hits("/knaben/")), (2, 1));
}

#[tokio::test]
async fn a_slow_answer_that_has_started_keeps_the_whole_deadline() {
    let slow = Reply { stall: Duration::from_millis(2200), ..ok(fixture("apibay/q.php-ubuntu.json")) };
    let server = Server::start(vec![("/apibay/", vec![slow])]).await;
    let search = Search::new(&server.config(&["apibay"]), ResultCache::new()).unwrap();

    let result = search.search(&query("ubuntu", &["apibay"])).await.unwrap();

    assert_eq!(outcomes(&result), [("apibay".into(), SourceOutcome::Ok { results: 100 })]);
    assert_eq!(server.hits("/apibay/"), 1);
}

#[tokio::test]
async fn a_response_that_never_starts_is_retried_once() {
    let stalled = Reply { delay: Duration::from_secs(10), ..ok(Vec::new()) };
    let server = Server::start(vec![("/apibay/", vec![stalled, ok(fixture("apibay/q.php-ubuntu.json"))])]).await;
    let search = Search::new(&server.config(&["apibay"]), ResultCache::new()).unwrap();

    let result = search.search(&query("ubuntu", &["apibay"])).await.unwrap();

    assert_eq!(outcomes(&result), [("apibay".into(), SourceOutcome::Ok { results: 100 })]);
    assert_eq!(server.hits("/apibay/"), 2);
}

#[tokio::test]
async fn a_slow_first_byte_is_raced_rather_than_abandoned() {
    let slow = Reply { delay: Duration::from_millis(2000), ..ok(fixture("apibay/q.php-ubuntu.json")) };
    let server = Server::start(vec![("/apibay/", vec![slow])]).await;
    let search = Search::new(&server.config(&["apibay"]), ResultCache::new()).unwrap();

    let result = search.search(&query("ubuntu", &["apibay"])).await.unwrap();

    assert_eq!(outcomes(&result), [("apibay".into(), SourceOutcome::Ok { results: 100 })]);
    assert!(result.elapsed_ms < 2600, "took {} ms", result.elapsed_ms);
}

#[tokio::test]
async fn a_short_budget_waits_for_its_only_request() {
    let slow = Reply { delay: Duration::from_millis(750), ..ok(fixture("apibay/q.php-ubuntu.json")) };
    let server = Server::start(vec![("/apibay/", vec![slow])]).await;
    let search = Search::new(&server.config(&["apibay"]), ResultCache::new()).unwrap();
    let mut q = query("ubuntu", &["apibay"]);
    q.timeout_ms = 1200;

    let result = search.search(&q).await.unwrap();

    assert_eq!(outcomes(&result), [("apibay".into(), SourceOutcome::Ok { results: 100 })]);
    assert_eq!(server.hits("/apibay/"), 1);
}

#[tokio::test]
async fn torznab_is_asked_only_once() {
    let server =
        Server::start(vec![("/torznab/api", vec![status(503), ok(fixture("torznab/jackett-ubuntu.xml"))])]).await;
    let config = SearchConfig {
        torznab: vec![TorznabConfig {
            name: "jackett".into(),
            url: format!("{}/torznab/api", server.base),
            api_key: None,
        }],
        ..SearchConfig::default()
    };
    let search = Search::new(&config, ResultCache::new()).unwrap();

    let err = search.search(&query("ubuntu", &["jackett"])).await.unwrap_err();

    let Some(Details::Sources(reports)) = err.details else { panic!("no source reports") };
    assert_eq!(reports[0].outcome, SourceOutcome::Http { status: 503 });
    assert_eq!(server.hits("/torznab/api"), 1);
}

#[tokio::test]
async fn an_oversized_answer_is_an_error_not_a_truncated_list() {
    let mut huge = fixture("apibay/q.php-ubuntu.json");
    huge.resize(5 << 20, b' ');
    let server = Server::start(vec![
        ("/apibay/", vec![Reply { content_length: false, ..ok(huge) }]),
        ("/knaben/", vec![ok(fixture("knaben/v2-search-ubuntu.json"))]),
    ])
    .await;
    let search = Search::new(&server.config(&["knaben", "apibay"]), ResultCache::new()).unwrap();

    let result = search.search(&query("ubuntu", &["knaben", "apibay"])).await.unwrap();

    assert_eq!(
        outcomes(&result)[1],
        ("apibay".into(), SourceOutcome::BadResponse { reason: "response larger than 4194304 bytes".into() })
    );
}

#[tokio::test]
async fn a_slow_source_times_out_by_the_deadline() {
    let slow = Reply { delay: Duration::from_secs(10), ..ok(fixture("knaben/v2-search-ubuntu.json")) };
    let server =
        Server::start(vec![("/knaben/", vec![slow]), ("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])])
            .await;
    let search = Search::new(&server.config(&["knaben", "apibay"]), ResultCache::new()).unwrap();
    let mut q = query("ubuntu", &["knaben", "apibay"]);
    q.timeout_ms = 1500;

    let started = Instant::now();
    let result = search.search(&q).await.unwrap();
    let wall = started.elapsed();

    assert_eq!(outcomes(&result)[0], ("knaben".into(), SourceOutcome::Timeout));
    assert_eq!(outcomes(&result)[1], ("apibay".into(), SourceOutcome::Ok { results: 100 }));
    assert!(wall < Duration::from_millis(1700), "returned after {wall:?}");
    assert!(result.elapsed_ms < 1700, "reported {} ms", result.elapsed_ms);
}

#[tokio::test]
async fn a_repeat_search_is_served_from_the_cache_unless_fresh() {
    let server = Server::start(vec![
        ("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))]),
        ("/knaben/", vec![status(503)]),
    ])
    .await;
    let search = Search::new(&server.config(&["knaben", "apibay"]), ResultCache::new()).unwrap();
    let q = query("Ubuntu", &["knaben", "apibay"]);

    let first = search.search(&q).await.unwrap();
    let mut narrower = query("ubuntu", &["knaben", "apibay"]);
    narrower.min_seeds = Some(30);
    let second = search.search(&narrower).await.unwrap();
    let fresh = search.search(&SearchQuery { fresh: true, ..q.clone() }).await.unwrap();

    assert_eq!(outcomes(&first)[1], ("apibay".into(), SourceOutcome::Ok { results: 100 }));
    assert_eq!(outcomes(&second)[1], ("apibay".into(), SourceOutcome::Cached { results: 100 }));
    let seeded: Vec<(&str, Option<u32>)> = second.releases.iter().map(|r| (r.name.as_str(), r.seeders)).collect();
    assert_eq!(seeded, [("ubuntu-24.04.1-desktop-amd64.iso", Some(51)), ("Ubuntu 22.04 LTS", Some(37))]);
    assert_eq!(outcomes(&fresh)[1], ("apibay".into(), SourceOutcome::Ok { results: 100 }));
    assert_eq!((server.hits("/apibay/"), server.hits("/knaben/")), (2, 6));
}

#[tokio::test]
async fn a_shared_cache_does_not_answer_for_another_endpoint() {
    let first = Server::start(vec![("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])]).await;
    let mirror = Server::start(vec![("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])]).await;
    let cache = ResultCache::new();
    let q = query("ubuntu", &["apibay"]);

    Search::new(&first.config(&["apibay"]), cache.clone()).unwrap().search(&q).await.unwrap();
    let moved = Search::new(&mirror.config(&["apibay"]), cache).unwrap().search(&q).await.unwrap();

    assert_eq!(outcomes(&moved), [("apibay".into(), SourceOutcome::Ok { results: 100 })]);
    assert_eq!(mirror.hits("/apibay/"), 1);
}

#[tokio::test]
async fn torznab_keys_stay_out_of_results_but_resolve_for_the_engine() {
    let server = Server::start(vec![("/torznab/api", vec![ok(fixture("torznab/jackett-ubuntu.xml"))])]).await;
    let config = SearchConfig {
        torznab: vec![TorznabConfig {
            name: "jackett".into(),
            url: format!("{}/torznab/api", server.base),
            api_key: Some("k3y".into()),
        }],
        ..SearchConfig::default()
    };
    let cache = ResultCache::new();
    let search = Search::new(&config, cache.clone()).unwrap();

    let result = search.search(&query("ubuntu", &["jackett"])).await.unwrap();

    assert_eq!(result.releases.len(), 2);
    assert!(!format!("{result:?}").contains("k3y"));
    assert!(!serde_json::to_string(&result).unwrap().contains("k3y"));
    let locator = result.releases.iter().find(|r| r.id.to_ref().starts_with('u')).unwrap();
    let resolved = cache.find(&locator.id.to_ref().parse().unwrap());
    assert_eq!(
        resolved[0].torrent_url.as_deref(),
        Some("http://127.0.0.1:9117/dl/privatehd/?jackett_apikey=k3y&path=def&file=ubuntu")
    );
}

#[tokio::test]
async fn a_new_torznab_key_is_not_answered_from_the_old_one() {
    let server = Server::start(vec![("/torznab/api", vec![ok(fixture("torznab/jackett-ubuntu.xml"))])]).await;
    let config = |key: &str| SearchConfig {
        torznab: vec![TorznabConfig {
            name: "jackett".into(),
            url: format!("{}/torznab/api", server.base),
            api_key: Some(key.into()),
        }],
        ..SearchConfig::default()
    };
    let cache = ResultCache::new();
    let q = query("ubuntu", &["jackett"]);

    Search::new(&config("old"), cache.clone()).unwrap().search(&q).await.unwrap();
    let rekeyed = Search::new(&config("new"), cache).unwrap().search(&q).await.unwrap();

    assert_eq!(outcomes(&rekeyed), [("jackett".into(), SourceOutcome::Ok { results: 3 })]);
    assert_eq!(server.hits("/torznab/api"), 2);
}

#[tokio::test]
async fn returned_releases_resolve_by_id_prefix() {
    let server = Server::start(vec![("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])]).await;
    let cache = ResultCache::new();
    let search = Search::new(&server.config(&["apibay"]), cache.clone()).unwrap();

    search.search(&query("ubuntu 22.04", &["apibay"])).await.unwrap();

    let prefix: HexPrefix = "2c6b68".parse().unwrap();
    let found: Vec<String> = cache.find(&prefix).into_iter().map(|r| r.name).collect();
    assert_eq!(found, ["Ubuntu 22.04 LTS"]);
    assert_eq!(cache.find(&"ffff00".parse().unwrap()), []);
}

#[tokio::test]
async fn official_torrents_get_their_hash_or_a_locator() {
    let listing = fixture("debian/debian-cd-current-arm64-bt-cd.html");
    let torrent = fixture("debian/debian-cd-current-arm64-bt-cd-debian-13.7.0-arm64-netinst.iso.torrent");
    let dir = "/debian/debian-cd/current/arm64/bt-cd/";
    let file = "/debian/debian-cd/current/arm64/bt-cd/debian-13.7.0-arm64-netinst.iso.torrent";
    let hashed = Server::start(vec![(file, vec![ok(torrent)]), (dir, vec![ok(listing.clone())])]).await;
    let broken = Server::start(vec![(file, vec![status(500)]), (dir, vec![ok(listing)])]).await;
    let q = query("debian 13 arm64 netinst", &["debian"]);

    let found = Search::new(&hashed.config(&["debian"]), ResultCache::new()).unwrap().search(&q).await.unwrap();
    let fallback = Search::new(&broken.config(&["debian"]), ResultCache::new()).unwrap().search(&q).await.unwrap();

    let release = &found.releases[0];
    assert_eq!(release.id.to_ref(), "bf7ed99351f7b08f0916b7b3e49cfa574d206879");
    assert_eq!(release.name, "debian-13.7.0-arm64-netinst.iso");
    assert_eq!(release.size, Some(735_358_976));
    assert_eq!(release.flags.iter().collect::<Vec<_>>(), [&Flag::Official]);
    assert_eq!(
        release.magnet.as_deref(),
        Some(
            "magnet:?xt=urn:btih:bf7ed99351f7b08f0916b7b3e49cfa574d206879&dn=debian-13.7.0-arm64-netinst.iso\
             &tr=http%3A%2F%2Fbttracker.debian.org%3A6969%2Fannounce"
        )
    );
    let locator = &fallback.releases[0];
    assert!(locator.id.to_ref().starts_with('u'));
    assert_eq!(locator.torrent_url.as_deref(), Some(format!("{}{file}", broken.base).as_str()));
    assert_eq!(locator.magnet, None);
}

#[tokio::test]
async fn a_block_page_is_not_an_empty_listing() {
    let block = ok(b"<html><title>Access denied</title><body>blocked by policy</body></html>".to_vec());
    let server = Server::start(vec![("/debian/", vec![block])]).await;
    let search = Search::new(&server.config(&["debian"]), ResultCache::new()).unwrap();

    let err = search.search(&query("debian 13", &["debian"])).await.unwrap_err();

    assert_eq!(err.code, ErrorCode::ProvidersUnavailable);
    let Some(Details::Sources(reports)) = err.details else { panic!("no source reports") };
    assert_eq!(reports[0].outcome, SourceOutcome::BadResponse { reason: "not a directory listing".into() });
}

const ARM64_CD: &str = "/debian/debian-cd/current/arm64/bt-cd/";
const NETINST: &str = "/debian/debian-cd/current/arm64/bt-cd/debian-13.7.0-arm64-netinst.iso.torrent";

fn netinst() -> Vec<u8> {
    fixture("debian/debian-cd-current-arm64-bt-cd-debian-13.7.0-arm64-netinst.iso.torrent")
}

#[tokio::test]
async fn a_locator_fallback_is_shown_but_asked_again() {
    let server = Server::start(vec![
        (NETINST, vec![status(500), status(500), ok(netinst())]),
        (ARM64_CD, vec![ok(fixture("debian/debian-cd-current-arm64-bt-cd.html"))]),
        ("/debian/debian-cd/current/arm64/bt-dvd/", vec![ok(fixture("debian/debian-cd-current-arm64-bt-dvd.html"))]),
        ("/debian/debian-cd/current/amd64/bt-cd/", vec![ok(fixture("debian/debian-cd-current-amd64-bt-cd.html"))]),
        ("/debian/debian-cd/current/amd64/bt-dvd/", vec![ok(fixture("debian/debian-cd-current-amd64-bt-dvd.html"))]),
    ])
    .await;
    let search = Search::new(&server.config(&["debian"]), ResultCache::new()).unwrap();
    let q = query("debian 13 arm64 netinst", &["debian"]);

    let degraded = search.search(&q).await.unwrap();
    let repaired = search.search(&q).await.unwrap();

    assert!(degraded.releases[0].id.to_ref().starts_with('u'));
    assert_eq!(outcomes(&repaired), [("debian".into(), SourceOutcome::Ok { results: 1 })]);
    assert_eq!(repaired.releases[0].id.to_ref(), "bf7ed99351f7b08f0916b7b3e49cfa574d206879");
}

#[tokio::test]
async fn an_answer_missing_a_directory_is_asked_again() {
    let server = Server::start(vec![
        (NETINST, vec![ok(netinst())]),
        (ARM64_CD, vec![ok(fixture("debian/debian-cd-current-arm64-bt-cd.html"))]),
        ("/debian/", vec![status(503)]),
    ])
    .await;
    let search = Search::new(&server.config(&["debian"]), ResultCache::new()).unwrap();
    let q = query("debian 13 arm64 netinst", &["debian"]);

    let first = search.search(&q).await.unwrap();
    let again = search.search(&q).await.unwrap();

    assert_eq!(first.releases[0].id.to_ref(), "bf7ed99351f7b08f0916b7b3e49cfa574d206879");
    assert_eq!(outcomes(&again), [("debian".into(), SourceOutcome::Ok { results: 1 })]);
    assert_eq!(server.hits(ARM64_CD), 2);
}

#[tokio::test]
async fn the_plain_image_outranks_bigger_variants() {
    let torrents = [
        "amd64/bt-cd/debian-13.7.0-amd64-netinst.iso.torrent",
        "amd64/bt-cd/debian-edu-13.7.0-amd64-netinst.iso.torrent",
        "amd64/bt-cd/debian-mac-13.7.0-amd64-netinst.iso.torrent",
        "arm64/bt-cd/debian-13.7.0-arm64-netinst.iso.torrent",
    ];
    let mut routes = Vec::new();
    for path in torrents.into_iter().chain(["amd64/bt-cd/", "amd64/bt-dvd/", "arm64/bt-cd/", "arm64/bt-dvd/"]) {
        let name = path.replace('/', "-");
        let page = if path.ends_with('/') { format!("{}.html", name.trim_end_matches('-')) } else { name };
        let route: &'static str = format!("/debian/debian-cd/current/{path}").leak();
        routes.push((route, vec![ok(fixture(&format!("debian/debian-cd-current-{page}")))]));
    }
    let server = Server::start(routes).await;
    let search = Search::new(&server.config(&["debian"]), ResultCache::new()).unwrap();

    let result = search.search(&query("debian 13.7 netinst", &["debian"])).await.unwrap();

    let listed: Vec<(&str, Option<u64>)> = result.releases.iter().map(|r| (r.name.as_str(), r.size)).collect();
    assert_eq!(
        listed,
        [
            ("debian-13.7.0-amd64-netinst.iso", Some(792_723_456)),
            ("debian-13.7.0-arm64-netinst.iso", Some(735_358_976)),
            ("debian-edu-13.7.0-amd64-netinst.iso", Some(801_112_064)),
            ("debian-mac-13.7.0-amd64-netinst.iso", Some(793_772_032)),
        ]
    );
}

#[tokio::test]
async fn a_plain_title_outranks_bigger_remasters() {
    let answer = fixture("archive.org/advancedsearch.php-night-of-the-living-dead-1968.json");
    let server = Server::start(vec![("/archive.org/advancedsearch.php", vec![ok(answer)])]).await;
    let search = Search::new(&server.config(&["archive.org"]), ResultCache::new()).unwrap();
    let q = SearchQuery { category: Some(Category::Video), ..query("night of the living dead 1968", &["archive.org"]) };

    let result = search.search(&q).await.unwrap();

    let top: Vec<(&str, Option<u64>)> = result.releases.iter().take(4).map(|r| (r.name.as_str(), r.size)).collect();
    assert_eq!(
        top,
        [
            ("Night of the Living Dead (1968)", Some(1_173_905_628)),
            ("Night of the Living Dead (1968)", Some(1_173_905_516)),
            ("Night of the Living Dead (1968)", Some(553_635_842)),
            ("Night of the Living Dead (1968)", Some(1_965_163_851)),
        ]
    );
}

#[tokio::test]
async fn a_stale_catalog_is_shown_but_asked_again() {
    let dir = scratch_dir("stale");
    let up =
        Server::start(vec![("/academictorrents/database.xml", vec![ok(fixture("academictorrents/database.xml"))])])
            .await;
    let down = Server::start(vec![("/academictorrents/", vec![status(503)])]).await;
    let q = query("stack exchange", &["academictorrents"]);
    let with_dir =
        |server: &Server| SearchConfig { cache_dir: Some(dir.clone()), ..server.config(&["academictorrents"]) };
    Search::new(&with_dir(&up), ResultCache::new()).unwrap().search(&q).await.unwrap();
    let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
    std::fs::File::options()
        .write(true)
        .open(dir.join("academictorrents.xml"))
        .unwrap()
        .set_modified(two_days_ago)
        .unwrap();

    let search = Search::new(&with_dir(&down), ResultCache::new()).unwrap();
    let stale = search.search(&q).await.unwrap();
    let again = search.search(&q).await.unwrap();

    assert_eq!(stale.releases[0].name, "Stack Exchange Data Dump (2026-09-30)");
    assert_eq!(outcomes(&again), [("academictorrents".into(), SourceOutcome::Ok { results: 2 })]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn the_academic_catalog_survives_a_restart() {
    let dir = scratch_dir("academic");
    let up =
        Server::start(vec![("/academictorrents/database.xml", vec![ok(fixture("academictorrents/database.xml"))])])
            .await;
    let down = Server::start(vec![("/academictorrents/", vec![status(503)])]).await;
    let q = query("stack exchange", &["academictorrents"]);
    let with_dir =
        |server: &Server| SearchConfig { cache_dir: Some(dir.clone()), ..server.config(&["academictorrents"]) };

    let first = Search::new(&with_dir(&up), ResultCache::new()).unwrap().search(&q).await.unwrap();
    let after_restart = Search::new(&with_dir(&down), ResultCache::new()).unwrap().search(&q).await.unwrap();

    let listed = |r: &torrential_core::SearchResult| -> Vec<(String, String)> {
        r.releases.iter().map(|r| (r.id.to_ref(), r.name.clone())).collect()
    };
    assert_eq!(
        listed(&first),
        [
            ("c32cdd0306ee7717cd63f98f8df6486af55f79c8".into(), "Stack Exchange Data Dump (2026-09-30)".into()),
            ("794176de24c33cc0e8b629b45a746cd870f3e95f".into(), "Stack Exchange Data Dump (2026-06-30)".into()),
        ]
    );
    assert_eq!(listed(&after_restart), listed(&first));
    assert_eq!(down.hits("/academictorrents/"), 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn progress_shows_each_answer_and_the_final_ranking_is_unchanged() {
    let late = |body: Vec<u8>, ms: u64| Reply { delay: Duration::from_millis(ms), ..ok(body) };
    let server = Server::start(vec![
        ("/knaben/", vec![ok(fixture("knaben/v2-search-ubuntu.json"))]),
        ("/apibay/", vec![late(fixture("apibay/q.php-ubuntu.json"), 300)]),
        ("/nyaa/", vec![Reply { delay: Duration::from_millis(600), ..status(404) }]),
    ])
    .await;
    let sources = ["knaben", "apibay", "nyaa"];
    let mut q = query("ubuntu", &sources);
    q.limit = 200;
    let streamed = Search::new(&server.config(&sources), ResultCache::new()).unwrap();
    let mut updates: Vec<Progress> = Vec::new();

    let last = streamed.search_with_progress(&q, || q.profile.clone(), |p| updates.push(p)).await.unwrap();
    let whole = Search::new(&server.config(&sources), ResultCache::new()).unwrap().search(&q).await.unwrap();

    let seen: Vec<String> = updates
        .iter()
        .map(|p| format!("{:?} total {} pending {:?}", outcomes(&p.result), p.result.total, p.pending))
        .collect();
    assert_eq!(
        seen,
        [
            r#"[("knaben", Ok { results: 98 })] total 97 pending ["apibay", "nyaa"]"#,
            r#"[("knaben", Ok { results: 98 }), ("apibay", Ok { results: 100 })] total 158 pending ["nyaa"]"#,
        ]
    );
    assert_eq!(outcomes(&last)[2], ("nyaa".into(), SourceOutcome::Http { status: 404 }));
    assert_eq!(last.releases, whole.releases);
    assert_eq!(last.releases, updates[1].result.releases);
}

#[tokio::test]
async fn a_title_searches_each_of_its_queries_and_reports_each_source_that_lists_video_once() {
    let row = |hash: char, name: &str, seeders: u32, size: u64| {
        format!(
            r#"{{"id":"1","name":"{name}","info_hash":"{}","leechers":"1","seeders":"{seeders}","size":"{size}","num_files":"1","username":"u","added":"1600000000","status":"member","category":"207","imdb":""}}"#,
            hash.to_string().repeat(40)
        )
    };
    let bluray = row('a', "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", 40, 6_000_000_000);
    let dated = format!("[{bluray},{}]", row('b', "Night of the Living Dead (1990) 1080p BluRay", 90, 9_000_000_000));
    let yearless = format!("[{bluray},{}]", row('c', "Night.of.the.Living.Dead.720p.WEB.x264", 12, 2_000_000_000));
    let late = |body: String| Reply { delay: Duration::from_millis(300), ..ok(body.into_bytes()) };
    let server = Server::start(vec![
        ("/apibay/q.php?q=night%20of%20the%20living%20dead%201968&", vec![ok(dated.into_bytes())]),
        ("/apibay/q.php?q=night%20of%20the%20living%20dead&", vec![late(yearless)]),
        ("/knaben/", vec![status(404)]),
        ("/ubuntu/", vec![status(404)]),
        ("/academictorrents/", vec![status(404)]),
    ])
    .await;
    let sources = ["knaben", "apibay", "ubuntu", "academictorrents"];
    let wanted = Wanted {
        name: "Night of the Living Dead".into(),
        year: Some(1968),
        episode: None,
        namesakes: Namesakes::Leading,
        runtime: Some(Duration::from_secs(96 * 60)),
        season_episodes: None,
        countries: Default::default(),
    };
    let q = SearchQuery { wanted: Some(wanted), ..query("night of the living dead", &sources) };
    let search = Search::new(&server.config(&sources), ResultCache::new()).unwrap();
    let mut updates: Vec<Progress> = Vec::new();

    let result = search.search_with_progress(&q, || q.profile.clone(), |p| updates.push(p)).await.unwrap();

    let spans: Vec<(&str, String)> =
        result.releases.iter().map(|r| (r.name.as_str(), r.span.map(|s| s.to_string()).unwrap_or_default())).collect();
    assert_eq!(
        spans,
        [
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", "film".to_string()),
            ("Night.of.the.Living.Dead.720p.WEB.x264", "film".to_string()),
        ]
    );
    assert_eq!(
        outcomes(&result),
        [("knaben".into(), SourceOutcome::Http { status: 404 }), ("apibay".into(), SourceOutcome::Ok { results: 4 })]
    );
    assert_eq!(updates.last().map(|p| p.pending.clone()), Some(vec!["apibay".to_string()]));
    assert_eq!(server.hits("/apibay/q.php?q=night%20of%20the%20living%20dead%201968&"), 1);
    assert_eq!(server.hits("/apibay/q.php?q=night%20of%20the%20living%20dead&"), 1);
    assert_eq!(server.hits("/knaben/"), 2);
    assert_eq!((server.hits("/ubuntu/"), server.hits("/academictorrents/")), (0, 0), "they hold no films");
}

const UBUNTU_TORRENT: &str = "/ubuntu/24.04/ubuntu-24.04.5-live-server-amd64.iso.torrent";
const UBUNTU_PAGES: [(&str, &str); 6] = [
    ("/ubuntu/26.10/", "ubuntu/26.10.html"),
    ("/ubuntu/26.04/", "ubuntu/26.04.html"),
    ("/ubuntu/25.10/", "ubuntu/25.10.html"),
    ("/ubuntu/24.04/", "ubuntu/24.04.html"),
    ("/ubuntu/22.04/", "ubuntu/22.04.html"),
    ("/ubuntu/", "ubuntu/index.html"),
];

async fn ubuntu_server() -> Server {
    let mut routes = vec![(UBUNTU_TORRENT, vec![ok(netinst())])];
    for (path, page) in &UBUNTU_PAGES[..5] {
        let html = String::from_utf8(fixture(page)).unwrap();
        for file in
            html.split("href=\"").skip(1).filter_map(|s| s.split('"').next()).filter(|f| f.ends_with(".torrent"))
        {
            let route: &'static str = format!("{path}{file}").leak();
            if !routes.iter().any(|(r, _)| *r == route) {
                routes.push((route, vec![ok(named_torrent(file))]));
            }
        }
    }
    routes.extend(UBUNTU_PAGES.map(|(path, page)| (path, vec![ok(fixture(page))])));
    Server::start(routes).await
}

fn named_torrent(name: &str) -> Vec<u8> {
    let info =
        format!("d6:lengthi1048576e4:name{}:{name}12:piece lengthi262144e6:pieces80:{}e", name.len(), "z".repeat(80));
    format!("d4:info{info}e").into_bytes()
}

fn ubuntu_page_hits(server: &Server) -> [usize; 6] {
    UBUNTU_PAGES.map(|(path, _)| server.hits(path))
}

fn listed_with_size(r: &torrential_core::SearchResult) -> Vec<(String, String, Option<u64>)> {
    r.releases.iter().map(|r| (r.id.to_ref(), r.name.clone(), r.size)).collect()
}

fn live_server_2404() -> Vec<(String, String, Option<u64>)> {
    vec![(
        "bf7ed99351f7b08f0916b7b3e49cfa574d206879".to_string(),
        "ubuntu-24.04.5-live-server-amd64.iso".to_string(),
        Some(735_358_976),
    )]
}

#[tokio::test]
async fn a_hashed_torrent_is_not_fetched_again_by_a_later_process() {
    let dir = scratch_dir("hashes");
    let server = ubuntu_server().await;
    let config = SearchConfig { cache_dir: Some(dir.clone()), ..server.config(&["ubuntu"]) };
    let q = query("ubuntu 24.04.5 live server", &["ubuntu"]);

    let first = Search::new(&config, ResultCache::new()).unwrap().search(&q).await.unwrap();
    let later = Search::new(&config, ResultCache::new()).unwrap().search(&q).await.unwrap();

    assert_eq!(listed_with_size(&first), live_server_2404());
    assert_eq!(listed_with_size(&later), live_server_2404());
    assert_eq!(server.hits(UBUNTU_TORRENT), 1);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_later_process_answers_from_the_listing_pages_on_disk_unless_fresh() {
    let dir = scratch_dir("pages");
    let server = ubuntu_server().await;
    let config = SearchConfig { cache_dir: Some(dir.clone()), ..server.config(&["ubuntu"]) };
    let q = query("ubuntu 24.04.5 live server", &["ubuntu"]);
    Search::new(&config, ResultCache::new()).unwrap().search(&q).await.unwrap();

    let later = Search::new(&config, ResultCache::new()).unwrap();
    let from_disk = later.search(&q).await.unwrap();
    let hits_from_disk = ubuntu_page_hits(&server);
    let fresh = later.search(&SearchQuery { fresh: true, ..q.clone() }).await.unwrap();

    assert_eq!(listed_with_size(&from_disk), live_server_2404());
    assert_eq!(outcomes(&from_disk), [("ubuntu".into(), SourceOutcome::Ok { results: 1 })]);
    assert_eq!(hits_from_disk, [1; 6], "only the first process asked for the pages");
    assert_eq!(listed_with_size(&fresh), live_server_2404());
    assert_eq!(ubuntu_page_hits(&server), [2; 6], "a fresh search asked for every page");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_search_hashes_the_other_listed_releases_for_the_next_one() {
    let dir = scratch_dir("hash-later");
    let server = ubuntu_server().await;
    let config = SearchConfig { cache_dir: Some(dir.clone()), ..server.config(&["ubuntu"]) };
    Search::new(&config, ResultCache::new())
        .unwrap()
        .search(&query("ubuntu 24.04.5 live server", &["ubuntu"]))
        .await
        .unwrap();
    let saved = || -> usize {
        let body = std::fs::read(dir.join("torrent-hashes.json")).unwrap_or_default();
        serde_json::from_slice::<Vec<serde_json::Value>>(&body).map_or(0, |all| all.len())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while saved() < 10 {
        assert!(Instant::now() < deadline, "the other releases were not hashed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let later =
        Search::new(&config, ResultCache::new()).unwrap().search(&query("ubuntu 22.04", &["ubuntu"])).await.unwrap();

    let mut found: Vec<(bool, String)> =
        later.releases.iter().map(|r| (r.id.to_ref().starts_with('u'), r.name.clone())).collect();
    found.sort();
    assert_eq!(
        found,
        [
            (false, "ubuntu-22.04.5-desktop-amd64.iso".to_string()),
            (false, "ubuntu-22.04.5-live-server-amd64.iso".to_string())
        ],
        "both are hashed, neither is a locator"
    );
    let fetched = ["ubuntu-22.04.5-desktop-amd64.iso.torrent", "ubuntu-22.04.5-live-server-amd64.iso.torrent"]
        .map(|file| server.hits(&format!("/ubuntu/22.04/{file}")));
    assert_eq!(fetched, [1, 1], "each was fetched once, behind the first search");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_stale_listing_page_answers_at_once_and_is_fetched_again_behind_the_search() {
    let dir = scratch_dir("stale-pages");
    let server = ubuntu_server().await;
    let config = SearchConfig { cache_dir: Some(dir.clone()), ..server.config(&["ubuntu"]) };
    let q = query("ubuntu 24.04.5 live server", &["ubuntu"]);
    Search::new(&config, ResultCache::new()).unwrap().search(&q).await.unwrap();
    let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(48 * 3600);
    let pages: Vec<PathBuf> = std::fs::read_dir(dir.join("pages")).unwrap().map(|e| e.unwrap().path()).collect();
    for page in &pages {
        std::fs::File::options().write(true).open(page).unwrap().set_modified(two_days_ago).unwrap();
    }
    let renewed = || pages.iter().all(|p| std::fs::metadata(p).unwrap().modified().unwrap() > two_days_ago);

    let later = Search::new(&config, ResultCache::new()).unwrap();
    let stale = later.search(&q).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !renewed() {
        assert!(Instant::now() < deadline, "the stale pages were not fetched again");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let after_refresh = later.search(&q).await.unwrap();
    let repeat = later.search(&q).await.unwrap();

    assert_eq!(pages.len(), 6);
    assert_eq!(listed_with_size(&stale), live_server_2404());
    assert_eq!(ubuntu_page_hits(&server), [2; 6], "each stale page was fetched once more");
    assert_eq!(
        outcomes(&after_refresh),
        [("ubuntu".into(), SourceOutcome::Ok { results: 1 })],
        "the stale answer was not cached"
    );
    assert_eq!(outcomes(&repeat), [("ubuntu".into(), SourceOutcome::Cached { results: 1 })]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_disabled_source_is_neither_searched_nor_nameable() {
    let server = Server::start(vec![("/apibay/", vec![ok(fixture("apibay/q.php-ubuntu.json"))])]).await;
    let config = SearchConfig { disabled: vec!["knaben".into(), "nyaa".into()], ..server.config(&["apibay"]) };
    let search = Search::new(&config, ResultCache::new()).unwrap();

    let named = search.search(&query("ubuntu", &["apibay", "knaben"])).await.unwrap_err();
    let unknown = Search::new(&SearchConfig { disabled: vec!["1337x".into()], ..config.clone() }, ResultCache::new())
        .err()
        .unwrap();

    let names: Vec<String> = search.sources().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["archlinux", "ubuntu", "debian", "archive.org", "academictorrents", "apibay"]);
    assert_eq!((named.code, named.message.as_str()), (ErrorCode::Usage, "source \"knaben\" is disabled"));
    assert_eq!(server.hits("/apibay/"), 0);
    assert_eq!(unknown.message, "sources.disabled lists an unknown source \"1337x\"");
}

#[tokio::test]
async fn a_torrent_fetch_failure_never_quotes_the_link() {
    let torrent = netinst();
    let server = Server::start(vec![
        ("/dl/ok", vec![ok(torrent.clone())]),
        ("/dl/huge", vec![Reply { content_length: false, ..ok(vec![b'd'; 17 << 20]) }]),
    ])
    .await;
    let search = Search::new(&SearchConfig::default(), ResultCache::new()).unwrap();
    let link = |path: &str| format!("{}{path}?jackett_apikey=k3y", server.base);

    let fetched = search.fetch_torrent(&link("/dl/ok")).await.unwrap();
    let missing = search.fetch_torrent(&link("/dl/gone")).await.unwrap_err();
    let huge = search.fetch_torrent(&link("/dl/huge")).await.unwrap_err();
    let refused = search.fetch_torrent("http://127.0.0.1:9/dl?apikey=k3y").await.unwrap_err();

    assert_eq!(fetched, torrent);
    assert_eq!(
        (missing.code, missing.message.as_str()),
        (ErrorCode::FetchFailed, "could not fetch the .torrent file: HTTP 404")
    );
    assert_eq!(huge.message, "could not fetch the .torrent file: larger than 16 MiB");
    assert!(refused.message.starts_with("could not fetch the .torrent file: "), "{}", refused.message);
    for error in [&missing, &huge, &refused] {
        assert!(!serde_json::to_string(error).unwrap().contains("k3y"), "{error:?}");
    }
}

#[tokio::test]
async fn restored_results_resolve_without_replacing_fresh_links() {
    let server = Server::start(vec![("/torznab/api", vec![ok(fixture("torznab/jackett-ubuntu.xml"))])]).await;
    let config = SearchConfig {
        torznab: vec![TorznabConfig {
            name: "jackett".into(),
            url: format!("{}/torznab/api", server.base),
            api_key: Some("k3y".into()),
        }],
        ..SearchConfig::default()
    };
    let cache = ResultCache::new();
    let search = Search::new(&config, cache.clone()).unwrap();
    let result = search.search(&query("ubuntu", &["jackett"])).await.unwrap();
    let locator = result.releases.iter().find(|r| r.id.to_ref().starts_with('u')).unwrap().clone();
    let older = torrential_core::Release {
        id: torrential_core::ReleaseId::Torrent("9b521219bc48550140a666f74a29048ed78e933b".parse().unwrap()),
        name: "archlinux-2026.10.01-x86_64.iso".into(),
        ..locator.clone()
    };

    cache.restore(vec![(locator.clone(), None), (older, None)]);

    let private = cache.find(&locator.id.to_ref().parse().unwrap());
    assert_eq!(
        private[0].torrent_url.as_deref(),
        Some("http://127.0.0.1:9117/dl/privatehd/?jackett_apikey=k3y&path=def&file=ubuntu")
    );
    let restored: Vec<String> = cache.find(&"9b5212".parse().unwrap()).into_iter().map(|r| r.name).collect();
    assert_eq!(restored, ["archlinux-2026.10.01-x86_64.iso"]);
}

#[tokio::test]
async fn a_private_link_is_saved_without_its_key_and_takes_the_current_one_back() {
    let server = Server::start(vec![("/torznab/api", vec![ok(fixture("torznab/jackett-ubuntu.xml"))])]).await;
    let config = |key: Option<&str>| SearchConfig {
        torznab: vec![TorznabConfig {
            name: "jackett".into(),
            url: format!("{}/torznab/api", server.base),
            api_key: key.map(String::from),
        }],
        ..SearchConfig::default()
    };
    let cache = ResultCache::new();
    let search = Search::new(&config(Some("k3y")), cache.clone()).unwrap();
    let result = search.search(&query("ubuntu", &["jackett"])).await.unwrap();
    let locator = result.releases.iter().find(|r| r.id.to_ref().starts_with('u')).unwrap().clone();
    let saved = cache.saved_link(&locator.id);

    let next_engine = |config: SearchConfig| {
        let cache = ResultCache::new();
        cache.restore(vec![(locator.clone(), saved.clone())]);
        let found = cache.find(&locator.id.to_ref().parse().unwrap()).remove(0);
        Search::new(&config, cache).unwrap().torrent_url(&found)
    };
    let rotated = next_engine(config(Some("n3w")));
    let keyless = next_engine(config(None)).unwrap_err();
    let gone = next_engine(SearchConfig::default()).unwrap_err();

    assert_eq!(
        saved,
        Some(SavedLink {
            source: "jackett".into(),
            url: "http://127.0.0.1:9117/dl/privatehd/?path=def&file=ubuntu".into(),
            key_params: vec!["jackett_apikey".into()],
        })
    );
    assert_eq!(
        rotated.unwrap().as_deref(),
        Some("http://127.0.0.1:9117/dl/privatehd/?path=def&file=ubuntu&jackett_apikey=n3w")
    );
    assert_eq!(
        [keyless.message, gone.message],
        [
            "this result's .torrent link needs an API key for the jackett indexer, which has none",
            "this result's .torrent link needs an API key for the jackett indexer, which is not configured",
        ]
    );
}

#[tokio::test]
async fn rejects_unknown_sources_and_queries_without_words() {
    let search = Search::new(&SearchConfig::default(), ResultCache::new()).unwrap();

    let unknown = search.search(&query("ubuntu", &["1337x"])).await.unwrap_err();
    let empty = search.search(&SearchQuery::new(" -- ")).await.unwrap_err();

    assert_eq!((unknown.code, unknown.message.as_str()), (ErrorCode::Usage, "unknown source \"1337x\""));
    assert_eq!(
        unknown.hint.as_deref(),
        Some("available sources: archlinux, ubuntu, debian, archive.org, academictorrents, knaben, apibay, nyaa")
    );
    assert_eq!((empty.code, empty.message.as_str()), (ErrorCode::Usage, "a search needs at least one word"));
}

fn scratch_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("torrential-search-{name}-{}-{nanos}", std::process::id()))
}
