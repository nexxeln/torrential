//! Play on TV against a fake TV on this machine's address on the home network: it answers SSDP
//! searches and UPnP AV actions as a TV does, keeps each action it is sent, and fetches the video
//! it is told to play with HEAD and Range requests, as a TV does.

use std::net::{Ipv4Addr, SocketAddr};
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
}

#[derive(Clone)]
struct FakeTv {
    ssdp: SocketAddr,
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
        let tv = Self { ssdp: udp.local_addr().unwrap(), screen: Arc::new(Mutex::new(Screen::default())) };
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

/// Answers one request to the fake TV's HTTP server.
async fn answer(mut stream: TcpStream, screen: Arc<Mutex<Screen>>, sink: &'static str) {
    let mut request = Vec::new();
    let mut buf = [0; 4096];
    let head_end = loop {
        let Ok(read) = stream.read(&mut buf).await else { return };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buf[..read]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).to_string();
    let header = |name: &str| {
        head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim().to_string())
        })
    };
    let length: usize = header("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
    while request.len() < head_end + length {
        let Ok(read) = stream.read(&mut buf).await else { return };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&buf[..read]);
    }
    let body = String::from_utf8_lossy(&request[head_end..]).to_string();
    let reply = if head.starts_with("GET /description.xml") {
        DESCRIPTION.to_string()
    } else {
        let action = header("soapaction").unwrap_or_default();
        let action = action.trim_matches('"').rsplit('#').next().unwrap_or_default().to_string();
        let out = act(&screen, &action, &body, sink);
        screen.lock().unwrap().actions.push((action.clone(), body));
        format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">{out}</u:{action}Response>\
             </s:Body></s:Envelope>"
        )
    };
    let answer = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = stream.write_all(answer.as_bytes()).await;
}

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
            tokio::spawn(fetch(shown.uri.clone(), screen.clone()));
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

const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>Fake TV</friendlyName>
    <UDN>uuid:fake-tv</UDN>
    <serviceList>
      <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType><controlURL>/AVTransport/control</controlURL></service>
      <service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType><controlURL>/RenderingControl/control</controlURL></service>
      <service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType><controlURL>/ConnectionManager/control</controlURL></service>
    </serviceList>
  </device>
</root>"#;

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
            "id": "uuid:fake-tv", "name": "Fake TV", "address": ip.to_string(),
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
