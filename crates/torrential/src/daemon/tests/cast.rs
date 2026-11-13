//! Play on TV against a fake TV on this machine's address on the home network: it answers SSDP
//! searches and UPnP AV actions as a TV does, keeps each action it is sent, and fetches the video
//! it is told to play with HEAD and Range requests, as a TV does. A lying device stands for
//! anything else on the network that answers a search.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use torrential_core::protocol::{Request, Response};
use torrential_core::{Cast, CastAction, CastRequest, CastState, Error, ErrorCode, Target, TvList, VideoRef};

use super::super::cast::Find;
use super::super::devices::names::Names;
use super::super::search_port::{self, FakeSource};
use super::titles::listed;
use super::{GIB, Harness, MIB, Outside, prefix, torrent, written};

/// The machine's address on the home network, or `None` on one with no network to test over.
fn lan() -> Option<Ipv4Addr> {
    let found = Names::read().lan.first().copied();
    if found.is_none() {
        eprintln!("skipped: this machine has no address on a home network");
    }
    found
}

/// What the fake TV has been told, and what it is doing.
#[derive(Default)]
struct Screen {
    /// Each action's name and body, oldest first.
    actions: Vec<(String, String)>,
    uri: String,
    transport: &'static str,
    /// Seconds.
    position: f64,
    volume: u8,
    /// What its fetches of the video got: the request, the status, and the headers a TV reads.
    fetches: Vec<String>,
    /// The bytes of the last range it fetched.
    fetched: Vec<u8>,
    /// Each request's method and path, oldest first.
    requests: Vec<String>,
    /// Actions it answers with a UPnP fault.
    faults: Vec<&'static str>,
    /// Actions it hangs up on without an answer.
    hang_up: Vec<&'static str>,
    /// The next action of this name waits this long before the TV does it.
    slow: Option<(&'static str, Duration)>,
}

#[derive(Clone)]
struct FakeTv {
    ssdp: SocketAddr,
    http: SocketAddr,
    screen: Arc<Mutex<Screen>>,
}

/// What the fake TV says it plays: MP4 and MKV, the latter by a name of its own.
const SINK: &str =
    "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_HP_HD_AAC,http-get:*:video/x-mkv:*,http-get:*:audio/mpeg:*";

impl FakeTv {
    async fn start(ip: Ipv4Addr, sink: &'static str) -> Self {
        let http = TcpListener::bind((ip, 0)).await.unwrap();
        let location = format!("http://{}/description.xml", http.local_addr().unwrap());
        let udp = UdpSocket::bind((ip, 0)).await.unwrap();
        let screen = Arc::new(Mutex::new(Screen::default()));
        let tv = Self { ssdp: udp.local_addr().unwrap(), http: http.local_addr().unwrap(), screen };
        tv.screen.lock().unwrap().transport = "NO_MEDIA_PRESENT";
        tokio::spawn(async move {
            let mut buf = [0; 2048];
            while let Ok((length, from)) = udp.recv_from(&mut buf).await {
                if String::from_utf8_lossy(&buf[..length]).contains("ST: urn:schemas-upnp-org:device:MediaRenderer:1") {
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {location}\r\n\
                         ST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\
                         USN: uuid:fake-tv::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n"
                    );
                    let _ = udp.send_to(answer.as_bytes(), from).await;
                }
            }
        });
        let screen = tv.screen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = http.accept().await {
                tokio::spawn(answer(stream, screen.clone(), sink));
            }
        });
        tv
    }

    fn actions(&self) -> Vec<String> {
        self.screen.lock().unwrap().actions.iter().map(|(name, _)| name.clone()).collect()
    }

    /// The body of the last action named `name`.
    fn sent(&self, name: &str) -> String {
        let screen = self.screen.lock().unwrap();
        screen.actions.iter().rev().find(|(action, _)| action == name).map(|(_, body)| body.clone()).unwrap_or_default()
    }

    fn set(&self, change: impl FnOnce(&mut Screen)) {
        change(&mut self.screen.lock().unwrap());
    }

    async fn until(&self, what: &str, done: impl Fn(&Screen) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(&self.screen.lock().unwrap()) {
            assert!(Instant::now() < deadline, "the fake TV never saw {what}: {:?}", self.actions());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// One HTTP request: its head and its body, or `None` for a connection that closed first.
async fn request(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut request = Vec::new();
    let mut buf = [0; 4096];
    let head_end = loop {
        let read = stream.read(&mut buf).await.ok().filter(|read| *read > 0)?;
        request.extend_from_slice(&buf[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).to_string();
    let length: usize = header(&head, "content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    while request.len() < head_end + length {
        let read = stream.read(&mut buf).await.ok().filter(|read| *read > 0)?;
        request.extend_from_slice(&buf[..read]);
    }
    Some((head, String::from_utf8_lossy(&request[head_end..]).to_string()))
}

fn header(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim().to_string())
    })
}

/// The method and path of a request's head.
fn asked(head: &str) -> String {
    head.split(' ').take(2).collect::<Vec<_>>().join(" ")
}

/// Answers one request to the fake TV's HTTP server.
async fn answer(mut stream: TcpStream, screen: Arc<Mutex<Screen>>, sink: &'static str) {
    let Some((head, body)) = request(&mut stream).await else { return };
    screen.lock().unwrap().requests.push(asked(&head));
    let (status, reply) = if head.starts_with("GET /description.xml") {
        ("200 OK", description("Fake TV", "uuid:fake-tv", "", None))
    } else {
        let action = header(&head, "soapaction").unwrap_or_default();
        let action = action.trim_matches('"').rsplit('#').next().unwrap_or_default().to_string();
        let slow = screen.lock().unwrap().slow.take_if(|(name, _)| *name == action).map(|(_, wait)| wait);
        if let Some(wait) = slow {
            tokio::time::sleep(wait).await;
        }
        let (fault, hang_up) = {
            let shown = screen.lock().unwrap();
            (shown.faults.contains(&action.as_str()), shown.hang_up.contains(&action.as_str()))
        };
        if hang_up {
            screen.lock().unwrap().actions.push((action, body));
            return;
        }
        let out = if fault { String::new() } else { act(&screen, &action, &body, sink) };
        screen.lock().unwrap().actions.push((action.clone(), body));
        if fault {
            ("500 Internal Server Error", FAULT.to_string())
        } else {
            let reply = format!(
                "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">{out}</u:{action}Response>\
                 </s:Body></s:Envelope>"
            );
            ("200 OK", reply)
        }
    };
    let answer = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = stream.write_all(answer.as_bytes()).await;
}

/// What a TV answers to an action it cannot do now.
const FAULT: &str = "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
    <s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
    <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>501</errorCode>\
    <errorDescription>Action Failed</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

/// Does what an action asks, and returns the arguments of its answer.
fn act(screen: &Arc<Mutex<Screen>>, action: &str, body: &str, sink: &str) -> String {
    let argument = |name: &str| {
        let start = body.find(&format!("<{name}>"))? + name.len() + 2;
        Some(body[start..start + body[start..].find(&format!("</{name}>"))?].to_string())
    };
    let mut shown = screen.lock().unwrap();
    match action {
        "GetProtocolInfo" => return format!("<Source></Source><Sink>{sink}</Sink>"),
        "SetAVTransportURI" => {
            shown.uri = argument("CurrentURI").unwrap_or_default().replace("&amp;", "&");
            shown.transport = "STOPPED";
        }
        "Play" => {
            shown.transport = "PLAYING";
            if !shown.uri.is_empty() {
                tokio::spawn(fetch(shown.uri.clone(), screen.clone()));
            }
        }
        "Pause" => shown.transport = "PAUSED_PLAYBACK",
        "Stop" => shown.transport = "STOPPED",
        "Seek" => {
            let target = argument("Target").unwrap_or_default();
            shown.position =
                target.split(':').filter_map(|p| p.parse::<f64>().ok()).fold(0.0, |total, part| total * 60.0 + part);
        }
        "SetVolume" => shown.volume = argument("DesiredVolume").and_then(|v| v.parse().ok()).unwrap_or(0),
        "GetVolume" => return format!("<CurrentVolume>{}</CurrentVolume>", shown.volume),
        "GetTransportInfo" => {
            return format!(
                "<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus>\
                 <CurrentSpeed>1</CurrentSpeed>",
                shown.transport
            );
        }
        "GetPositionInfo" => {
            let whole = shown.position as u64;
            return format!(
                "<Track>1</Track><TrackDuration>1:36:00</TrackDuration><TrackURI>{}</TrackURI>\
                 <RelTime>{}:{:02}:{:02}</RelTime><AbsTime>NOT_IMPLEMENTED</AbsTime>",
                shown.uri.replace('&', "&amp;"),
                whole / 3600,
                whole / 60 % 60,
                whole % 60
            );
        }
        _ => {}
    }
    String::new()
}

/// Fetches the video as a TV does when it starts and then seeks: a HEAD, the start, and a later
/// range. Each goes in `fetches` as the request, the status, and the headers a TV reads.
async fn fetch(uri: String, screen: Arc<Mutex<Screen>>) {
    let url = url::Url::parse(&uri).unwrap();
    let host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
    for (method, range) in [("HEAD", None), ("GET", Some("bytes=0-")), ("GET", Some("bytes=1048576-1049599"))] {
        let (line, bytes) = fetch_one(&host, url.path(), method, range).await;
        let mut shown = screen.lock().unwrap();
        shown.fetches.push(line);
        if range == Some("bytes=1048576-1049599") {
            shown.fetched = bytes;
        }
    }
}

async fn fetch_one(host: &str, path: &str, method: &str, range: Option<&str>) -> (String, Vec<u8>) {
    let Ok(mut stream) = TcpStream::connect(host).await else { return (format!("{method} refused"), Vec::new()) };
    let range_header = range.map_or(String::new(), |range| format!("Range: {range}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: FakeTV/1.0 DLNADOC/1.50\r\n\
         getcontentFeatures.dlna.org: 1\r\ntransferMode.dlna.org: Streaming\r\n{range_header}Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let mut buf = [0; 64 * 1024];
    // A TV reads what it buffers and lets the rest go.
    while answer.len() < 256 * 1024 {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(read)) => answer.extend_from_slice(&buf[..read]),
        }
    }
    let Some(split) = answer.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (format!("{method} {} no answer", range.unwrap_or("whole")), Vec::new());
    };
    let head = String::from_utf8_lossy(&answer[..split]).to_string();
    let status = head.lines().next().unwrap_or_default().split(' ').nth(1).unwrap_or_default().to_string();
    let header = |name: &str| {
        head.lines()
            .find_map(|line| {
                line.split_once(": ").filter(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, v)| v.to_string())
            })
            .unwrap_or_else(|| "-".into())
    };
    let line = format!(
        "{method} {} -> {status} {} {} {} {}",
        range.unwrap_or("whole"),
        header("content-type"),
        header("content-range"),
        header("transferMode.dlna.org"),
        header("contentFeatures.dlna.org"),
    );
    (line, answer[split + 4..].to_vec())
}

/// A renderer's device description. Its services take actions at `controls` followed by their
/// paths, relative to `base` or to where the description is.
fn description(name: &str, udn: &str, controls: &str, base: Option<&str>) -> String {
    let base = base.map_or(String::new(), |base| format!("<URLBase>{base}</URLBase>"));
    format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">{base}
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>{name}</friendlyName>
    <UDN>{udn}</UDN>
    <serviceList>
      <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>{controls}/AVTransport/control</controlURL></service>
      <service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><controlURL>{controls}/RenderingControl/control</controlURL></service>
      <service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>{controls}/ConnectionManager/control</controlURL></service>
    </serviceList>
  </device>
</root>"#
    )
}

const RENDERER: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
/// Bytes in a description or an answer that never ends, as far as the engine is concerned.
const HUGE: usize = 50 << 20;

/// A device on the home network that lies to the engine. It answers each search with every
/// LOCATION and USN `answers` gives for its HTTP address, and serves `/redirect.xml`,
/// `/elsewhere.xml`, and `/based.xml`, which send the engine to `elsewhere`, `/huge.xml`, and
/// `/d/{n}.xml`, a renderer of its own for each `n`. Every action gets a huge answer.
struct Liar {
    ssdp: SocketAddr,
    count: Arc<Count>,
}

#[derive(Default)]
struct Count {
    /// Each request's method and path.
    requests: Mutex<Vec<String>>,
    open: AtomicUsize,
    most_open: AtomicUsize,
    /// The most bytes one answer got out before the engine hung up.
    most_sent: AtomicUsize,
}

impl Liar {
    async fn start(
        ip: Ipv4Addr,
        elsewhere: SocketAddr,
        answers: impl FnOnce(SocketAddr) -> Vec<(String, String)>,
    ) -> Self {
        let http = TcpListener::bind((ip, 0)).await.unwrap();
        let udp = UdpSocket::bind((ip, 0)).await.unwrap();
        let answers = answers(http.local_addr().unwrap());
        let liar = Self { ssdp: udp.local_addr().unwrap(), count: Arc::default() };
        tokio::spawn(async move {
            let mut buf = [0; 2048];
            while let Ok((_, from)) = udp.recv_from(&mut buf).await {
                for (location, usn) in &answers {
                    let datagram = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {location}\r\nST: {RENDERER}\r\nUSN: {usn}\r\n\r\n"
                    );
                    let _ = udp.send_to(datagram.as_bytes(), from).await;
                }
            }
        });
        let count = liar.count.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = http.accept().await {
                tokio::spawn(lie(stream, count.clone(), elsewhere));
            }
        });
        liar
    }

    fn requests(&self) -> Vec<String> {
        self.count.requests.lock().unwrap().clone()
    }
}

async fn lie(mut stream: TcpStream, count: Arc<Count>, elsewhere: SocketAddr) {
    let open = count.open.fetch_add(1, Ordering::SeqCst) + 1;
    count.most_open.fetch_max(open, Ordering::SeqCst);
    if let Some((head, _)) = request(&mut stream).await {
        let asked = asked(&head);
        count.requests.lock().unwrap().push(asked.clone());
        let other = format!("http://{elsewhere}");
        let (status, location, body, padding) = match asked.as_str() {
            "GET /redirect.xml" => ("302 Found", format!("Location: {other}/description.xml\r\n"), String::new(), 0),
            "GET /elsewhere.xml" => {
                ("200 OK", String::new(), description("Elsewhere", "uuid:elsewhere", &other, None), 0)
            }
            "GET /based.xml" => ("200 OK", String::new(), description("Based", "uuid:based", "", Some(&other)), 0),
            "GET /huge.xml" => ("200 OK", String::new(), description("Huge", "uuid:huge", "", None), HUGE),
            flood if flood.starts_with("GET /d/") => {
                let n = flood.trim_start_matches("GET /d/").trim_end_matches(".xml");
                tokio::time::sleep(Duration::from_millis(300)).await;
                ("200 OK", String::new(), description(&format!("Flood {n}"), &format!("uuid:flood-{n}"), "", None), 0)
            }
            action if action.starts_with("POST ") => {
                let answer = "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
                    <s:Body><u:GetProtocolInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:ConnectionManager:1\">\
                    <Source></Source><Sink>http-get:*:video/avi:*</Sink></u:GetProtocolInfoResponse></s:Body></s:Envelope>";
                ("200 OK", String::new(), answer.to_string(), HUGE)
            }
            _ => ("404 Not Found", String::new(), String::new(), 0),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n{location}Connection: close\r\n\r\n",
            body.len() + padding
        );
        let mut sent = 0;
        if stream.write_all(head.as_bytes()).await.is_ok() && stream.write_all(body.as_bytes()).await.is_ok() {
            sent = body.len();
            let spaces = vec![b' '; 64 * 1024];
            while sent < body.len() + padding {
                let part = spaces.len().min(body.len() + padding - sent);
                if stream.write_all(&spaces[..part]).await.is_err() {
                    break;
                }
                sent += part;
            }
        }
        count.most_sent.fetch_max(sent, Ordering::SeqCst);
    }
    count.open.fetch_sub(1, Ordering::SeqCst);
}

fn harness(tv: &FakeTv, search: search_port::Fake) -> Harness {
    Harness::with_outside(search, Outside { tvs: Find::At(tv.ssdp), ..Outside::default() })
}

async fn tvs(h: &Harness, fresh: bool) -> TvList {
    match h.ask(Request::Tvs { fresh }).await {
        Response::Tvs { list } => list,
        other => panic!("not a TV list: {other:?}"),
    }
}

async fn cast(h: &Harness, request: CastRequest) -> Result<Cast, Error> {
    match h.ask(Request::Cast(request)).await {
        Response::Cast { cast } => Ok(*cast),
        Response::Error { error } => Err(error),
        other => panic!("not a cast: {other:?}"),
    }
}

async fn control(h: &Harness, action: CastAction) -> Result<Cast, Error> {
    match h.ask(Request::CastControl { tv: None, action }).await {
        Response::Cast { cast } => Ok(*cast),
        Response::Error { error } => Err(error),
        other => panic!("not a cast: {other:?}"),
    }
}

/// Waits until the cast on the list is as `done` says.
async fn until(h: &Harness, what: &str, done: impl Fn(&Cast) -> bool) -> Cast {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let casts = tvs(h, false).await.casts;
        if let Some(cast) = casts.iter().find(|cast| done(cast)) {
            return cast.clone();
        }
        assert!(Instant::now() < deadline, "the cast never reached {what}: {casts:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn night() -> VideoRef {
    "tt0063350".parse().unwrap()
}

fn by_hand(target: Target) -> CastRequest {
    CastRequest { tv: None, target, source: None, file: None, video: None, subtitle: None, start_at: None }
}

/// Where the person stopped Night of the Living Dead, by the watch history.
fn kept(h: &Harness) -> Option<(f64, Option<f64>)> {
    let watched = h.running.ctx.titles.shelf.watched(&night().title).unwrap();
    watched.iter().find(|w| w.video == night()).map(|w| (w.position, w.duration))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_plays_a_download_on_the_tv_through_its_grant_and_the_grant_ends_with_it() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, SINK).await;
    let h = harness(&tv, search_port::Fake::new(Vec::new()));
    let film = torrent(80, "Night", &[("Night.mkv", 4 * MIB), ("Night.srt", 1_000)]);
    let bytes = written(&h.downloads().join("Night/Night.mkv"), 4 * MIB);
    written(&h.downloads().join("Night/Night.srt"), 1_000);
    h.swarm.set_rate(100 * GIB);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;

    let found = serde_json::to_value(&tvs(&h, true).await.tvs).unwrap();
    assert_eq!(
        found,
        serde_json::json!([{
            "id": format!("uuid:fake-tv@{ip}"), "name": "Fake TV", "address": ip.to_string(),
            "plays": ["mkv", "mp4"], "decodes": "h264", "volume": true,
        }])
    );

    let request = CastRequest {
        tv: Some("fake".into()),
        video: Some(night()),
        subtitle: Some(1),
        ..by_hand(Target::Prefix(prefix(&film)))
    };
    let playing = cast(&h, request).await.unwrap();
    assert_eq!(
        (playing.name.as_str(), &playing.state, playing.subtitle.as_deref(), playing.file),
        ("Night of the Living Dead (1968)", &CastState::Playing, Some("Night.srt"), Some(0))
    );
    assert_eq!(tv.actions()[..3], ["GetProtocolInfo", "SetAVTransportURI", "Play"]);
    let uri = tv.screen.lock().unwrap().uri.clone();
    let port = h.running.ctx.http_port;
    assert!(uri.starts_with(&format!("http://{ip}:{port}/cast/")) && uri.ends_with("/Night.mkv"), "{uri}");
    let metadata = tv.sent("SetAVTransportURI");
    for part in [
        "&lt;dc:title&gt;Night of the Living Dead (1968)&lt;/dc:title&gt;",
        "&lt;upnp:class&gt;object.item.videoItem.movie&lt;/upnp:class&gt;",
        "protocolInfo=&quot;http-get:*:video/x-mkv:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000&quot; size=&quot;4194304&quot;",
        "protocolInfo=&quot;http-get:*:text/srt:*&quot;",
        "&lt;sec:CaptionInfoEx sec:type=&quot;srt&quot;&gt;http://",
    ] {
        assert!(metadata.contains(part), "{part} is not in {metadata}");
    }

    tv.until("three fetches of the video", |screen| screen.fetches.len() == 3).await;
    let features = "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";
    assert_eq!(
        tv.screen.lock().unwrap().fetches,
        [
            format!("HEAD whole -> 200 video/x-mkv - Streaming {features}"),
            format!("GET bytes=0- -> 206 video/x-mkv bytes 0-4194303/4194304 Streaming {features}"),
            format!("GET bytes=1048576-1049599 -> 206 video/x-mkv bytes 1048576-1049599/4194304 Streaming {features}"),
        ]
    );
    assert_eq!(tv.screen.lock().unwrap().fetched, bytes[1_048_576..1_049_600]);

    tv.set(|screen| screen.position = 600.0);
    until(&h, "ten minutes in", |cast| cast.position == Some(600.0)).await;
    let paused = control(&h, CastAction::Pause).await.unwrap();
    assert_eq!((paused.state, tv.screen.lock().unwrap().transport), (CastState::Paused, "PAUSED_PLAYBACK"));
    assert_eq!(kept(&h), Some((600.0, Some(5_760.0))), "a pause keeps the position, as the web player does");
    let sought = control(&h, CastAction::Seek { position: 1_800.0 }).await.unwrap();
    assert_eq!(sought.position, Some(1_800.0));
    assert!(tv.sent("Seek").contains("<Unit>REL_TIME</Unit><Target>0:30:00</Target>"), "{}", tv.sent("Seek"));
    assert_eq!(control(&h, CastAction::Resume).await.unwrap().state, CastState::Playing);
    assert_eq!(control(&h, CastAction::Volume { level: 30 }).await.unwrap().volume, Some(30));
    assert!(tv.sent("SetVolume").contains("<Channel>Master</Channel><DesiredVolume>30</DesiredVolume>"));
    let late = control(&h, CastAction::Seek { position: 9_000.0 }).await.unwrap_err();
    assert_eq!((late.code, late.message.as_str()), (ErrorCode::Usage, "9000 is not a time in this video"));

    until(&h, "half an hour in", |cast| cast.position == Some(1_800.0)).await;
    let stopped = control(&h, CastAction::Stop).await.unwrap();
    assert_eq!(stopped.state, CastState::Ended { error: None });
    assert_eq!(tv.actions().last().map(String::as_str), Some("Stop"));
    assert_eq!(kept(&h), Some((1_800.0, Some(5_760.0))), "a stop keeps the position");
    let host = format!("{ip}:{port}");
    let path = uri.strip_prefix(&format!("http://{host}")).unwrap();
    let after = fetch_one(&host, path, "GET", Some("bytes=0-")).await.0;
    // With no grant live and no device paired, the engine stops listening on the network too.
    assert!(
        ["GET refused", "GET bytes=0- no answer", "GET bytes=0- -> 404 text/plain"]
            .iter()
            .any(|end| after.starts_with(end)),
        "the grant ended with the cast: {after}"
    );
    let ended = control(&h, CastAction::Pause).await.unwrap_err();
    assert_eq!((ended.code, ended.message.as_str()), (ErrorCode::NotFound, "no cast is playing"));
    assert_eq!(tvs(&h, false).await.casts.len(), 1, "an ended cast stays listed for a minute");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_stopped_on_the_tv_ends_and_keeps_where_it_stopped() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, SINK).await;
    let h = harness(&tv, search_port::Fake::new(Vec::new()));
    let film = torrent(81, "Night", &[("Night.mp4", 2 * MIB)]);
    written(&h.downloads().join("Night/Night.mp4"), 2 * MIB);
    h.swarm.set_rate(100 * GIB);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;

    let request = CastRequest { video: Some(night()), start_at: Some(90.0), ..by_hand(Target::Prefix(prefix(&film))) };
    cast(&h, request).await.unwrap();
    tv.until("the seek to where it starts", |screen| screen.position == 90.0).await;
    assert!(tv.sent("Seek").contains("<Target>0:01:30</Target>"));
    tv.set(|screen| screen.position = 1_200.0);
    until(&h, "twenty minutes in", |cast| cast.position == Some(1_200.0)).await;
    tv.set(|screen| screen.transport = "STOPPED");
    let ended = until(&h, "its end", Cast::ended).await;
    assert_eq!(ended.state, CastState::Ended { error: None });
    assert_eq!(kept(&h), Some((1_200.0, Some(5_760.0))));
    assert_eq!(tv.actions().iter().filter(|action| *action == "Stop").count(), 0, "the TV stopped it");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_video_in_a_container_the_tv_does_not_take_is_refused_before_it_downloads() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, "http-get:*:video/mp4:*").await;
    let h = harness(&tv, search_port::Fake::new(Vec::new()));
    let film = torrent(82, "Night", &[("Night.avi", 2 * MIB)]);
    let error = cast(&h, by_hand(h.torrent_file(&film))).await.unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str(), error.hint.as_deref()),
        (
            ErrorCode::Usage,
            "Fake TV does not play AVI files, only MP4",
            Some("play it on this Mac, or choose a source in a format the TV plays")
        )
    );
    assert_eq!(tv.actions(), ["GetProtocolInfo"]);
    assert!(h.status().await.is_empty(), "nothing downloads");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_title_cast_passes_over_a_source_the_tv_cannot_play_and_plays_the_next() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, SINK).await;
    let avi = torrent(0xb1, "Night.of.the.Living.Dead.1968.DVDRip.XviD", &[("Night.1968.avi", 700 * MIB)]);
    let mkv = torrent(0xb2, "Night.of.the.Living.Dead.1968.720p.x264", &[("Night.1968.720p.mkv", 2 * MIB)]);
    let catalog = vec![listed(&avi, "knaben", Some(90)), listed(&mkv, "knaben", Some(40))];
    let search = search_port::Fake::scripted(catalog, vec![FakeSource::answers("knaben", 0)], BTreeMap::new());
    let h = harness(&tv, search);
    for t in [&avi, &mkv] {
        h.swarm.publish(t.clone());
    }
    h.swarm.set_rate(100 * GIB);

    let playing = cast(&h, by_hand(Target::Title(night()))).await.unwrap();
    assert_eq!((playing.name.as_str(), playing.video), ("Night of the Living Dead (1968)", Some(night())));
    assert_eq!(playing.download.as_deref(), Some(mkv.info_hash.id().to_hex().as_str()));
    let uri = tv.screen.lock().unwrap().uri.clone();
    assert!(uri.ends_with("/Night.1968.720p.mkv"), "{uri}");
    tv.until("a fetch of the video", |screen| !screen.fetches.is_empty()).await;
    assert!(tv.screen.lock().unwrap().fetches[0].starts_with("HEAD whole -> 200 video/x-mkv"));
    control(&h, CastAction::Stop).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_sends_the_engine_to_loopback_or_another_host_is_never_followed_there() {
    let Some(ip) = lan() else { return };
    // A renderer on 127.0.0.1 stands for the engine's own loopback and for another host.
    let bystander = FakeTv::start(Ipv4Addr::LOCALHOST, SINK).await;
    let liar = Liar::start(ip, bystander.http, |at| {
        let bystander = bystander.http;
        vec![
            (
                format!("http://{bystander}/description.xml"),
                "uuid:loopback::urn:schemas-upnp-org:device:MediaRenderer:1".into(),
            ),
            (format!("http://{at}/redirect.xml"), "uuid:redirect::urn:schemas-upnp-org:device:MediaRenderer:1".into()),
            (
                format!("http://{at}/elsewhere.xml"),
                "uuid:elsewhere::urn:schemas-upnp-org:device:MediaRenderer:1".into(),
            ),
            (format!("http://{at}/based.xml"), "uuid:based::urn:schemas-upnp-org:device:MediaRenderer:1".into()),
        ]
    })
    .await;
    let h = Harness::with_outside(
        search_port::Fake::new(Vec::new()),
        Outside { tvs: Find::At(liar.ssdp), ..Outside::default() },
    );

    let found = tvs(&h, true).await.tvs;
    assert_eq!(found, [], "no renderer whose actions go to another address");
    assert_eq!(bystander.screen.lock().unwrap().requests, Vec::<String>::new(), "nothing reached the other address");
    let mut asked = liar.requests();
    asked.sort();
    assert_eq!(asked, ["GET /based.xml", "GET /elsewhere.xml", "GET /redirect.xml"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_of_answers_and_huge_descriptions_cannot_crowd_out_a_tv_or_fill_the_engine() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, SINK).await;
    let liar = Liar::start(ip, tv.http, |at| {
        let mut answers = vec![
            (
                format!("http://{}/description.xml", tv.http),
                "uuid:fake-tv::urn:schemas-upnp-org:device:MediaRenderer:1".into(),
            ),
            (format!("http://{at}/huge.xml"), "uuid:huge::urn:schemas-upnp-org:device:MediaRenderer:1".into()),
        ];
        answers.extend((0..300).map(|n| (format!("http://{at}/d/{n}.xml"), format!("uuid:flood-{n}::{RENDERER}"))));
        answers
    })
    .await;
    let h = Harness::with_outside(
        search_port::Fake::new(Vec::new()),
        Outside { tvs: Find::At(liar.ssdp), ..Outside::default() },
    );

    let found = tvs(&h, true).await.tvs;
    let listed: Vec<String> =
        found.iter().map(|tv| format!("{} {}", tv.name, serde_json::to_string(&tv.plays).unwrap())).collect();
    assert_eq!(
        listed,
        [r#"Fake TV ["mkv","mp4"]"#, r#"Flood 0 ["mkv","mp4"]"#, r#"Flood 1 ["mkv","mp4"]"#],
        "four answers from one address at most, a huge description refused, and a huge answer to GetProtocolInfo unread"
    );
    let descriptions = liar.requests().iter().filter(|asked| asked.starts_with("GET ")).count();
    assert_eq!(descriptions, 3);
    assert!(liar.count.most_open.load(Ordering::SeqCst) <= 4);
    let sent = liar.count.most_sent.load(Ordering::SeqCst);
    assert!(sent < 16 << 20, "the engine hung up on a huge answer after {sent} bytes");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cast_that_takes_the_tv_waits_until_the_one_before_lets_go() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, SINK).await;
    let h = harness(&tv, search_port::Fake::new(Vec::new()));
    let film = torrent(84, "Night", &[("Night.mkv", 2 * MIB), ("Extra.mkv", 2 * MIB)]);
    written(&h.downloads().join("Night/Night.mkv"), 2 * MIB);
    written(&h.downloads().join("Night/Extra.mkv"), 2 * MIB);
    h.swarm.set_rate(100 * GIB);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;
    tvs(&h, true).await;

    tv.set(|screen| screen.slow = Some(("SetAVTransportURI", Duration::from_secs(2))));
    let first = CastRequest { file: Some(0), ..by_hand(Target::Prefix(prefix(&film))) };
    let second = CastRequest { file: Some(1), ..by_hand(Target::Prefix(prefix(&film))) };
    let both = async {
        tokio::join!(cast(&h, first), async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cast(&h, second).await
        })
    };
    let (first, second) = tokio::time::timeout(Duration::from_secs(30), both).await.expect("both casts answer");
    assert_eq!(first.unwrap().state, CastState::Ended { error: None }, "the first cast left the TV to the second");
    let second = second.unwrap();
    assert_eq!((second.state, second.file), (CastState::Playing, Some(1)));
    let sets: Vec<String> = {
        let screen = tv.screen.lock().unwrap();
        let sent = screen.actions.iter().filter(|(action, _)| action == "SetAVTransportURI");
        sent.map(|(_, body)| if body.contains("Extra.mkv") { "Extra" } else { "Night" }.to_string()).collect()
    };
    assert_eq!(sets, ["Night", "Extra"], "the late SetAVTransportURI of the first cast came before the second's");
    tv.until("the second video on the screen", |screen| screen.uri.ends_with("/Extra.mkv")).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(tv.screen.lock().unwrap().uri.ends_with("/Extra.mkv"), "and stays there");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tv_that_hangs_up_on_the_new_video_is_never_told_to_play() {
    let Some(ip) = lan() else { return };
    let tv = FakeTv::start(ip, SINK).await;
    let h = harness(&tv, search_port::Fake::new(Vec::new()));
    let film = torrent(85, "Night", &[("Night.mp4", 2 * MIB)]);
    written(&h.downloads().join("Night/Night.mp4"), 2 * MIB);
    h.swarm.set_rate(100 * GIB);
    h.get(h.torrent_file(&film), &[]).await.unwrap();
    h.until("done", |jobs| jobs[0].status.name() == "done").await;

    tv.set(|screen| screen.hang_up.push("SetAVTransportURI"));
    let error = cast(&h, by_hand(Target::Prefix(prefix(&film)))).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::NetworkUnavailable);
    assert!(error.message.starts_with("Fake TV cannot be reached"), "{}", error.message);
    assert_eq!(tv.actions(), ["GetProtocolInfo", "SetAVTransportURI"], "no Play to bring back what the TV had");
}
