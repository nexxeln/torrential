//! Phones and TVs reaching the engine over the home network: this Mac's own address on the
//! network, so each request arrives from a network peer, as a phone's would.

use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use torrential_core::protocol::{Request, Response};
use torrential_core::{DeviceList, ErrorCode};

use super::super::devices::names::Names;
use super::super::http;
use super::{GIB, Harness, MIB, torrent, written};

const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148";

/// The machine's address on the home network, or `None` on one with no network to test over.
fn lan() -> Option<Ipv4Addr> {
    let found = Names::read().lan.first().copied();
    if found.is_none() {
        eprintln!("skipped: this machine has no address on a home network");
    }
    found
}

struct Answer {
    status: u16,
    headers: String,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|line| line.strip_prefix(&format!("{name}: ")))
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn error(&self) -> (u16, String, String) {
        let error = &self.json()["error"];
        (self.status, error["code"].as_str().unwrap().into(), error["message"].as_str().unwrap().into())
    }
}

/// A browser on another machine: it reaches the engine by `ip`, names `host`, and keeps the
/// cookie the engine gives it.
struct Browser {
    ip: IpAddr,
    port: u16,
    host: String,
    cookie: Option<String>,
}

impl Browser {
    fn on(ip: Ipv4Addr, h: &Harness) -> Self {
        let port = h.running.ctx.http_port;
        Self { ip: IpAddr::V4(ip), port, host: format!("{ip}:{port}"), cookie: None }
    }

    async fn send(&self, method: &str, path: &str, headers: &[&str], body: &str) -> Answer {
        let mut stream = TcpStream::connect((self.ip, self.port)).await.unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", self.host);
        for header in headers {
            request.push_str(&format!("{header}\r\n"));
        }
        if let Some(cookie) = &self.cookie {
            request.push_str(&format!("Cookie: theme=dark; {cookie}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await.unwrap().unwrap();
        let split = answer.windows(4).position(|w| w == b"\r\n\r\n").expect("a whole head");
        let head = String::from_utf8_lossy(&answer[..split]).into_owned();
        let mut lines = head.lines();
        let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
        let mut headers: Vec<String> = lines
            .map(|line| {
                let (k, v) = line.split_once(": ").unwrap();
                format!("{}: {v}\n", k.to_ascii_lowercase())
            })
            .collect();
        headers.sort();
        Answer { status, headers: headers.concat(), body: answer[split + 4..].to_vec() }
    }

    async fn get(&self, path: &str) -> Answer {
        self.send("GET", path, &[], "").await
    }

    /// Pairs with `code`, keeping the cookie the engine sets.
    async fn pair(&mut self, code: &str) -> Answer {
        let answer = self
            .send("POST", "/api/pair", &[&format!("User-Agent: {IPHONE}")], &format!(r#"{{"code":"{code}"}}"#))
            .await;
        if let Some(cookie) = answer.header("set-cookie") {
            self.cookie = Some(cookie.split(';').next().unwrap().to_string());
        }
        answer
    }
}

async fn devices(h: &Harness, request: Request) -> DeviceList {
    match h.ask(request).await {
        Response::Devices { list } => list,
        other => panic!("not a device list: {other:?}"),
    }
}

/// Waits until the engine listens on `ip`, or until it stops.
async fn listening(h: &Harness, ip: Ipv4Addr, on: bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect((ip, h.running.ctx.http_port)).await.is_ok() != on {
        assert!(
            Instant::now() < deadline,
            "the engine never {} listening on {ip}",
            if on { "started" } else { "stopped" }
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_reaches_nothing_until_it_pairs_and_nothing_once_it_is_removed() {
    let Some(ip) = lan() else { return };
    let h = Harness::new();
    let film = torrent(70, "film", &[("film.mp4", 2 * MIB)]);
    written(&h.downloads().join("film/film.mp4"), 2 * MIB);
    h.swarm.set_rate(100 * GIB);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    let file = format!("/{}/0/film.mp4", film.info_hash.id().to_hex());
    listening(&h, ip, false).await;

    let offer = devices(&h, Request::AddDevice).await.offer.unwrap();
    listening(&h, ip, true).await;
    let mut phone = Browser::on(ip, &h);
    let not_paired = (
        401,
        "not_paired".to_string(),
        "this device is not paired with the Mac, so open /pair and enter the code that Add a device shows".to_string(),
    );
    for path in ["/api/jobs", "/api/home", "/api/devices", "/api/me", "/api/search?q=sintel", "/api/stream?id=x&file=0"]
    {
        assert_eq!(phone.get(path).await.error(), not_paired, "{path}");
    }
    let refused = phone.send("POST", "/api/remove", &[], r#"{"refs":["all"]}"#).await;
    assert_eq!(refused.error(), not_paired);
    for path in ["/", "/downloads", file.as_str(), "/assets/index.js", "/title/tt0063350"] {
        let page = phone.get(path).await;
        assert_eq!((page.status, page.header("location")), (303, Some("/pair")), "{path}");
    }
    let mut idle = TcpStream::connect((ip, h.running.ctx.http_port)).await.unwrap();
    let page = phone.get("/pair").await;
    assert_eq!(page.status, 200);
    assert!(page.text().contains("Enter the code that Add a device shows in torrential on the Mac."));

    let wrong = format!("{:06}", (offer.code.parse::<u32>().unwrap() + 1) % 1_000_000);
    assert_eq!(phone.pair(&wrong).await.status, 400);
    assert_eq!(phone.cookie, None);
    let paired = phone.pair(&offer.code).await;
    assert_eq!(paired.status, 200);
    let cookie = paired.header("set-cookie").unwrap();
    assert!(cookie.ends_with("; Path=/; Max-Age=34560000; HttpOnly; SameSite=Strict"), "{cookie}");
    assert_eq!(paired.json()["device"]["name"], "iPhone");

    assert_eq!(phone.get("/api/jobs").await.json()["jobs"][0]["name"], "film");
    assert_eq!(phone.get("/api/me").await.json()["device"]["name"], "iPhone");
    let home = phone.get("/").await;
    assert_eq!((home.status, home.header("set-cookie").is_some()), (200, true), "a visit renews the cookie");
    let bytes = phone.send("GET", &file, &["Range: bytes=0-99"], "").await;
    assert_eq!((bytes.status, bytes.body.len()), (206, 100));
    let mac_only =
        (403, "local_only".to_string(), "only the Mac running torrential can add or remove devices".to_string());
    assert_eq!(phone.get("/api/devices").await.error(), mac_only);
    assert_eq!(phone.send("POST", "/api/devices/add", &[], "{}").await.error(), mac_only);
    assert_eq!(phone.get("/pair").await.header("location"), Some("/"));

    let id = paired.json()["device"]["id"].as_str().unwrap().to_string();
    let events = {
        let mut stream = TcpStream::connect((phone.ip, phone.port)).await.unwrap();
        let cookie = phone.cookie.clone().unwrap();
        let request = format!("GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: {cookie}\r\n\r\n", phone.host);
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut first = [0; 64];
        let read = stream.read(&mut first).await.unwrap();
        assert!(first[..read].starts_with(b"HTTP/1.1 200 OK"));
        stream
    };
    let left = devices(&h, Request::RemoveDevice { id }).await;
    assert_eq!(left.devices, []);
    let mut events = events;
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), events.read_to_end(&mut rest))
        .await
        .expect("removing the phone ends the events it was receiving")
        .unwrap();
    listening(&h, ip, false).await;
    let mut nothing = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), idle.read_to_end(&mut nothing))
        .await
        .expect("a connection from the network closes with the network")
        .unwrap();

    devices(&h, Request::AddDevice).await;
    listening(&h, ip, true).await;
    let removed = (
        401,
        "not_paired".to_string(),
        "this device was removed on the Mac, so open /pair and enter a new code from Add a device".to_string(),
    );
    assert_eq!(phone.get("/api/jobs").await.error(), removed);
    assert_eq!(phone.get(&file).await.header("location"), Some("/pair"));
    assert!(phone.get("/pair").await.text().contains("This device was removed, so enter a new code"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_from_the_network_must_name_this_mac_and_come_from_its_own_page() {
    let Some(ip) = lan() else { return };
    let h = Harness::new();
    let offer = devices(&h, Request::AddDevice).await.offer.unwrap();
    listening(&h, ip, true).await;
    let port = h.running.ctx.http_port;
    let mut phone = Browser::on(ip, &h);
    phone.pair(&offer.code).await;
    assert_eq!(phone.get("/api/jobs").await.status, 200);

    let name = h.running.ctx.devices.local_name().unwrap();
    let by_name = Browser { host: format!("{name}:{port}"), ..Browser::on(ip, &h) };
    assert_eq!(by_name.get("/pair").await.status, 200, "the .local name");
    let elsewhere =
        format!("torrential answers only at its own address, so open http://{name}:{port} on this device instead.\n");
    for host in
        [format!("rebound.example:{port}"), format!("{ip}:{}", port + 1), ip.to_string(), format!("127.0.0.1:{port}")]
    {
        let page =
            Browser { host: host.clone(), cookie: phone.cookie.clone(), ..Browser::on(ip, &h) }.get("/api/jobs").await;
        assert_eq!((page.status, page.text()), (403, elsewhere.clone()), "{host}");
    }
    let own_origin = format!("Origin: http://{ip}:{port}");
    assert_eq!(phone.send("POST", "/api/pause", &[&own_origin], r#"{"refs":["x"]}"#).await.status, 404);
    for headers in [
        vec!["Sec-Fetch-Site: cross-site".to_string()],
        vec!["Sec-Fetch-Site: same-site".to_string()],
        vec!["Origin: https://example.org".to_string()],
        vec![format!("Origin: http://{ip}:3000")],
        vec!["Origin: null".to_string()],
    ] {
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        let answer = phone.send("POST", "/api/pause", &headers, r#"{"refs":["x"]}"#).await;
        assert_eq!(answer.status, 403, "{headers:?}");
    }
    let form = phone.send("POST", "/api/pair", &["Content-Type: text/plain"], "").await;
    assert_eq!(form.status, 415, "a cross-site form cannot send JSON");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_grant_serves_its_one_file_to_a_tv_until_it_is_dropped() {
    let Some(ip) = lan() else { return };
    let h = Harness::new();
    let film = torrent(71, "film", &[("film.mp4", 4 * MIB), ("other.mp4", MIB)]);
    let bytes = written(&h.downloads().join("film/film.mp4"), 4 * MIB);
    written(&h.downloads().join("film/other.mp4"), MIB);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("downloading", |jobs| jobs[0].status.name() == "downloading").await;
    let id = film.info_hash.id();

    let waiting = h.running.ctx.devices.grant(id, 0, "film.mp4").unwrap();
    listening(&h, ip, true).await;
    let tv = Browser::on(ip, &h);
    let mut playing = TcpStream::connect((ip, tv.port)).await.unwrap();
    let request = format!("GET {} HTTP/1.1\r\nHost: {}\r\n\r\n", waiting.path(), tv.host);
    playing.write_all(request.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(waiting);
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), playing.read_to_end(&mut received))
        .await
        .expect("dropping the grant ends the stream the TV was waiting on")
        .unwrap();
    assert!(received.len() < MIB as usize, "no piece had arrived");
    listening(&h, ip, false).await;

    h.swarm.set_rate(100 * GIB);
    let grant = h.running.ctx.devices.grant(id, 0, "film.mp4").unwrap();
    listening(&h, ip, true).await;
    let range = tv.send("GET", grant.path(), &["Range: bytes=1000-1099"], "").await;
    assert_eq!((range.status, range.body.as_slice()), (206, &bytes[1000..1100]));
    let head = tv.send("HEAD", grant.path(), &[], "").await;
    assert_eq!((head.status, head.header("content-length")), (200, Some("4194304")));
    let query = tv.get(&format!("{}?/api/jobs", grant.path())).await;
    assert_eq!((query.status, query.body.len()), (200, 4 * MIB as usize), "a query changes nothing a grant serves");
    assert_eq!(tv.send("POST", grant.path(), &[], "{}").await.status, 405);

    let token = grant.path().strip_prefix("/cast/").unwrap().split('/').next().unwrap();
    let ended = "This cast has ended, so press Play on TV again.\n";
    for path in [
        format!("/cast/{token}/other.mp4"),
        format!("/cast/{token}/film.mp4/"),
        format!("/cast/{token}/..%2Fother.mp4"),
        format!("/cast/{token}/../1/other.mp4"),
        format!("/cast/{}/film.mp4", token.to_uppercase()),
        format!("/cast/{}/film.mp4", "0".repeat(64)),
    ] {
        let answer = tv.get(&path).await;
        assert_eq!((answer.status, answer.text().as_str()), (404, ended), "{path}");
    }
    let other = tv.get(&format!("/{}/1/other.mp4", id.to_hex())).await;
    assert_eq!(other.header("location"), Some("/pair"), "the grant reaches no other file");
    assert_eq!(tv.get("/api/jobs").await.status, 401);

    let path = grant.path().to_string();
    let offer = devices(&h, Request::AddDevice).await.offer.unwrap();
    drop(grant);
    let after = tv.get(&path).await;
    assert_eq!((after.status, after.text().as_str()), (404, ended), "while a code keeps the network open");
    let mut phone = Browser::on(ip, &h);
    let id = phone.pair(&offer.code).await.json()["device"]["id"].as_str().unwrap().to_string();
    devices(&h, Request::RemoveDevice { id }).await;
    listening(&h, ip, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_code_opened_while_the_engine_starts_listening_still_opens_it_to_the_network() {
    let Some(ip) = lan() else { return };
    let mut h = Harness::new();
    h.running.http.abort();
    let _ = (&mut h.running.http).await;
    let ctx = h.running.ctx.clone();
    let reach = ctx.devices.reach();
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, ctx.http_port)).await.unwrap();
    devices(&h, Request::AddDevice).await;
    tokio::spawn(http::accept(ctx, listener, reach));
    listening(&h, ip, true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn this_mac_alone_adds_and_removes_devices_and_loopback_keeps_its_checks() {
    let h = Harness::new();
    let port = h.running.ctx.http_port;
    let mac = Browser { ip: IpAddr::V4(Ipv4Addr::LOCALHOST), port, host: format!("127.0.0.1:{port}"), cookie: None };
    assert_eq!(mac.get("/api/me").await.json(), serde_json::json!({ "asker": "mac" }));
    assert_eq!(mac.get("/api/devices").await.json(), serde_json::json!({ "devices": [], "offer": null }));
    let opened = mac.send("POST", "/api/devices/add", &[], "{}").await.json();
    let rows = opened["offer"]["qr"].as_array().unwrap();
    assert!(rows.len() >= 21 && rows.iter().all(|row| row.as_str().unwrap().len() == rows.len()));
    assert_eq!(
        opened["offer"]["url"],
        format!(
            "http://{}/pair#{}",
            opened["offer"]["address"].as_str().unwrap(),
            opened["offer"]["code"].as_str().unwrap()
        )
    );
    let missing = mac.send("POST", "/api/devices/remove", &[], r#"{"id":"0badf00d"}"#).await;
    assert_eq!(missing.error(), (404, "not_found".into(), "no device has the ID 0badf00d".into()));
    assert_eq!(mac.get("/pair").await.header("location"), Some("/"));
    let rebound = Browser { host: format!("rebound.example:{port}"), ..mac };
    assert_eq!(rebound.get("/api/devices").await.status, 403);
    let cross = mac_again(port).send("POST", "/api/devices/add", &["Sec-Fetch-Site: cross-site"], "{}").await;
    assert_eq!(cross.status, 403);
    let form = mac_again(port).send("POST", "/api/devices/add", &["Content-Type: text/plain"], "").await;
    assert_eq!(form.status, 415);
    if let Some(name) = h.running.ctx.devices.local_name() {
        let by_name = Browser { host: format!("{name}:{port}"), ..mac_again(port) };
        let files = "/api/files?ref=http%3A%2F%2Fexample.org%2Fsintel.torrent";
        let moved = by_name.get(files).await;
        let location = format!("http://127.0.0.1:{port}{files}");
        assert_eq!(
            (moved.status, moved.header("location"), moved.body.len()),
            (307, Some(location.as_str()), 0),
            "its own .local name goes to 127.0.0.1, where browsers say which site asked"
        );
        let followed = mac_again(port).send("GET", files, &["Sec-Fetch-Site: cross-site"], "").await;
        assert_eq!(followed.status, 403, "a browser that follows it from another website says so");
        let pause = by_name.send("POST", "/api/pause", &[], r#"{"refs":["all"]}"#).await;
        assert_eq!(
            (pause.status, pause.header("location")),
            (307, Some(format!("http://127.0.0.1:{port}/api/pause").as_str()))
        );
    }
    assert_eq!(ErrorCode::LocalOnly.exit_code(), 6);
}

fn mac_again(port: u16) -> Browser {
    Browser { ip: IpAddr::V4(Ipv4Addr::LOCALHOST), port, host: format!("127.0.0.1:{port}"), cookie: None }
}
