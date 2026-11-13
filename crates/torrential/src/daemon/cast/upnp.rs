//! A media renderer: what its device description says, what its ConnectionManager says it plays,
//! and the SOAP actions that drive its AVTransport and RenderingControl services.

use std::net::Ipv4Addr;
use std::time::Duration;

use torrential_core::{Container, Error, ErrorCode, Playback, Plays, Tv};
use url::Url;

use super::ssdp::at;
use super::xml;

/// How long a TV may take to answer one request. A TV that is off answers none.
const ANSWER_WITHIN: Duration = Duration::from_secs(4);
/// The most of a description or an answer torrential reads. A TV's run to a few kilobytes.
const READ_AT_MOST: usize = 256 * 1024;
const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:";
const RENDERING: &str = "urn:schemas-upnp-org:service:RenderingControl:";
const CONNECTION: &str = "urn:schemas-upnp-org:service:ConnectionManager:";
/// What a TV whose ConnectionManager says nothing plays: MKV and MP4 with H.264, as nearly every
/// TV sold since 2012 does.
const UNSAID: [Container; 2] = [Container::Mkv, Container::Mp4];

/// One of a renderer's services: its type with the version the device gave, and where to send
/// actions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Service {
    pub kind: String,
    pub control: Url,
}

/// A media renderer found on the home network.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Renderer {
    pub tv: Tv,
    pub transport: Service,
    pub rendering: Option<Service>,
    /// What its ConnectionManager said it plays, each as the MIME type and the fourth field of a
    /// protocolInfo. Empty when it said nothing.
    pub sink: Vec<(String, String)>,
}

/// What a device description says about a renderer, before its ConnectionManager is asked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Description {
    pub udn: String,
    pub name: String,
    pub transport: Service,
    pub rendering: Option<Service>,
    pub connection: Option<Service>,
}

/// Reads a device description fetched from the device at `from`. A service that takes actions
/// anywhere else is left out, and a device left with no AVTransport is `None`, since it cannot
/// play what it is sent.
pub fn describe(xml_text: &str, location: &Url, from: Ipv4Addr) -> Option<Description> {
    let base =
        xml::text(xml_text, "URLBase").and_then(|base| Url::parse(&base).ok()).unwrap_or_else(|| location.clone());
    let services: Vec<Service> = xml::all(xml_text, "service")
        .into_iter()
        .filter_map(|service| {
            let kind = xml::text(service, "serviceType")?;
            let control = base.join(&xml::text(service, "controlURL")?).ok().filter(|url| at(url, from))?;
            Some(Service { kind, control })
        })
        .collect();
    let find = |prefix: &str| services.iter().find(|s| s.kind.starts_with(prefix)).cloned();
    let name = xml::text(xml_text, "friendlyName").filter(|name| !name.is_empty());
    Some(Description {
        udn: xml::text(xml_text, "UDN").filter(|udn| !udn.is_empty())?,
        name: name.or_else(|| xml::text(xml_text, "modelName")).unwrap_or_else(|| "TV".into()),
        transport: find(AV_TRANSPORT)?,
        rendering: find(RENDERING),
        connection: find(CONNECTION),
    })
}

/// The protocolInfo entries of a `Sink`, as MIME type and fourth field, for HTTP only.
pub fn sink(list: &str) -> Vec<(String, String)> {
    list.split(',')
        .filter_map(|entry| {
            let mut fields = entry.trim().splitn(4, ':');
            let (protocol, _, mime) = (fields.next()?, fields.next()?, fields.next()?);
            (protocol.eq_ignore_ascii_case("http-get"))
                .then(|| (mime.trim().to_ascii_lowercase(), fields.next().unwrap_or("*").trim().to_string()))
        })
        .collect()
}

impl Renderer {
    pub fn new(description: Description, address: String, sink: Vec<(String, String)>) -> Self {
        let tv = Tv {
            id: format!("{}@{address}", description.udn),
            name: description.name,
            address,
            plays: plays(&sink),
            decodes: decodes(&sink),
            volume: description.rendering.is_some(),
        };
        Self { tv, transport: description.transport, rendering: description.rendering, sink }
    }

    /// The MIME type to send `container` as, in the TV's own spelling.
    pub fn mime(&self, container: Container) -> &str {
        let listed = self.sink.iter().find(|(mime, extra)| takes(mime, extra) == Some(container));
        listed.map_or(container.mimes()[0], |(mime, _)| mime)
    }
}

/// Parts of a DLNA media format profile's name that say its container. TVs list MPEG transport
/// streams as `video/mpeg` with a profile such as `AVC_TS_HD_24_AC3_T`, and program streams as
/// `video/mpeg` too.
const PROFILE_PARTS: [(&str, Container); 5] = [
    ("TS", Container::Ts),
    ("PS", Container::Mpeg),
    ("MPEG1", Container::Mpeg),
    ("MP4", Container::Mp4),
    ("MKV", Container::Mkv),
];

/// The container a sink entry takes: the one its DLNA profile names, or else its MIME type's.
fn takes(mime: &str, extra: &str) -> Option<Container> {
    let profile = extra.split(';').find_map(|field| field.trim().strip_prefix("DLNA.ORG_PN="));
    let named = profile.filter(|_| mime.starts_with("video/")).and_then(|profile| {
        profile
            .to_ascii_uppercase()
            .split('_')
            .find_map(|part| PROFILE_PARTS.iter().find(|(name, _)| *name == part).map(|(_, container)| *container))
    });
    named.or_else(|| Container::of_mime(mime))
}

/// The containers a sink takes. A sink that names `*` or `video/*` takes every one, and one that
/// names no video at all is taken to play what nearly every TV does.
fn plays(sink: &[(String, String)]) -> Plays {
    if sink.iter().any(|(mime, _)| mime == "*" || mime == "video/*") {
        return Plays::from_iter([
            Container::Mkv,
            Container::Mp4,
            Container::WebM,
            Container::Avi,
            Container::Ts,
            Container::Mov,
            Container::Mpeg,
            Container::Wmv,
        ]);
    }
    let named: Plays = sink.iter().filter_map(|(mime, extra)| takes(mime, extra)).collect();
    if named == Plays::default() && !sink.iter().any(|(mime, _)| mime.starts_with("video/")) {
        return UNSAID.into_iter().collect();
    }
    named
}

/// HEVC when the sink names an HEVC profile or type, and H.264 otherwise, which every TV decodes.
/// A TV that decodes HEVC without saying so gets H.264 sources, which it plays too.
fn decodes(sink: &[(String, String)]) -> Playback {
    let hevc = sink.iter().any(|(mime, extra)| {
        matches!(mime.as_str(), "video/hevc" | "video/h265" | "video/x-h265")
            || extra.to_ascii_uppercase().contains("HEVC")
    });
    if hevc { Playback::Hevc } else { Playback::H264 }
}

/// Why a SOAP action did not happen.
#[derive(Debug)]
pub enum Failed {
    /// The TV answered with this UPnP error code, such as 710 for a seek mode it lacks.
    Refused(u32, Error),
    /// The TV did not answer.
    Gone(Error),
}

impl From<Failed> for Error {
    fn from(failed: Failed) -> Error {
        match failed {
            Failed::Refused(_, error) | Failed::Gone(error) => error,
        }
    }
}

/// Sends one SOAP action and returns the answer's body. `tv` names the TV in errors.
pub async fn call(
    http: &reqwest::Client,
    service: &Service,
    action: &str,
    args: &[(&str, &str)],
    tv: &str,
) -> Result<String, Failed> {
    let arguments: String =
        args.iter().map(|(name, value)| format!("<{name}>{}</{name}>", xml::escape(value))).collect();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action} xmlns:u=\"{kind}\">{arguments}</u:{action}></s:Body></s:Envelope>",
        kind = service.kind,
    );
    let sent = http
        .post(service.control.clone())
        .timeout(ANSWER_WITHIN)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPAction", format!("\"{}#{action}\"", service.kind))
        .body(body)
        .send()
        .await
        .map_err(|e| gone(tv, &e))?;
    let status = sent.status();
    let text = read(sent).await.map_err(|e| gone(tv, &e))?.ok_or_else(|| {
        Failed::Gone(Error::new(ErrorCode::FetchFailed, format!("{tv} sent an answer too long to read")))
    })?;
    if status.is_success() {
        return Ok(text);
    }
    let code = xml::text(&text, "errorCode").and_then(|code| code.parse().ok()).unwrap_or(u32::from(status.as_u16()));
    let why =
        xml::text(&text, "errorDescription").filter(|d| !d.is_empty()).map_or(String::new(), |d| format!(": {d}"));
    Err(Failed::Refused(
        code,
        Error::new(ErrorCode::FetchFailed, format!("{tv} refused {action} with error {code}{why}")),
    ))
}

/// The text of an answer, read as it arrives. `None` once it runs past `READ_AT_MOST`.
pub async fn read(mut answer: reqwest::Response) -> Result<Option<String>, reqwest::Error> {
    if answer.content_length().is_some_and(|length| length > READ_AT_MOST as u64) {
        return Ok(None);
    }
    let mut body = Vec::new();
    while let Some(chunk) = answer.chunk().await? {
        if body.len() + chunk.len() > READ_AT_MOST {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(String::from_utf8_lossy(&body).into_owned()))
}

fn gone(tv: &str, error: &reqwest::Error) -> Failed {
    let why = if error.is_timeout() { "it did not answer in time".to_string() } else { error.to_string() };
    Failed::Gone(
        Error::new(ErrorCode::NetworkUnavailable, format!("{tv} cannot be reached ({why})"))
            .with_hint("check that the TV is on and on the same network as this Mac"),
    )
}

/// `H:MM:SS`, as AVTransport writes times.
pub fn clock(seconds: f64) -> String {
    let whole = seconds.max(0.0).round() as u64;
    format!("{}:{:02}:{:02}", whole / 3600, whole / 60 % 60, whole % 60)
}

/// Seconds from `H:MM:SS` or `H:MM:SS.fff`, or `None` for `NOT_IMPLEMENTED` and the like.
pub fn seconds(clock: &str) -> Option<f64> {
    let mut parts = clock.trim().split(':');
    let (hours, minutes, rest) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let seconds: f64 = rest.split('/').next()?.parse().ok()?;
    let total =
        hours.trim_start_matches('+').parse::<f64>().ok()? * 3600.0 + minutes.parse::<f64>().ok()? * 60.0 + seconds;
    total.is_finite().then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LG: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:dlna="urn:schemas-dlna-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>[LG] webOS TV OLED55C1</friendlyName>
    <manufacturer>LG Electronics</manufacturer>
    <modelName>OLED55C1PUB</modelName>
    <UDN>uuid:6a3e0b2c-0000-1000-8000-a823fe2b1c10</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/AVTransport/d2f9/control.xml</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/ConnectionManager/d2f9/control.xml</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>http://192.168.1.40:1876/RenderingControl/d2f9/control.xml</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    const TV: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 40);

    #[test]
    fn a_description_gives_the_name_and_where_each_service_takes_actions() {
        let location = Url::parse("http://192.168.1.40:1876/desc.xml").unwrap();
        let described = describe(LG, &location, TV).unwrap();
        assert_eq!(described.udn, "uuid:6a3e0b2c-0000-1000-8000-a823fe2b1c10");
        assert_eq!(described.name, "[LG] webOS TV OLED55C1");
        assert_eq!(described.transport.kind, "urn:schemas-upnp-org:service:AVTransport:1");
        assert_eq!(described.transport.control.as_str(), "http://192.168.1.40:1876/AVTransport/d2f9/control.xml");
        let rendering = described.rendering.unwrap();
        assert_eq!(rendering.control.as_str(), "http://192.168.1.40:1876/RenderingControl/d2f9/control.xml");
        let connection = described.connection.unwrap();
        assert_eq!(connection.control.as_str(), "http://192.168.1.40:1876/ConnectionManager/d2f9/control.xml");
        let speaker = LG.replace("AVTransport", "ContentDirectory");
        assert_eq!(describe(&speaker, &location, TV), None, "a device with no AVTransport plays nothing it is sent");
    }

    #[test]
    fn a_service_that_takes_actions_on_another_address_is_left_out() {
        let location = Url::parse("http://192.168.1.40:1876/desc.xml").unwrap();
        let elsewhere =
            LG.replace("http://192.168.1.40:1876/RenderingControl", "http://127.0.0.1:47919/RenderingControl");
        let described = describe(&elsewhere, &location, TV).unwrap();
        assert_eq!(described.rendering, None);
        assert!(described.connection.is_some());
        let based = LG.replace("<device>", "<URLBase>http://192.168.1.1/</URLBase><device>");
        assert_eq!(describe(&based, &location, TV), None, "every relative control URL is on the router now");
        let named = LG.replace("/AVTransport/d2f9", "http://tv.local:1876/AVTransport/d2f9");
        assert_eq!(describe(&named, &location, TV), None);
    }

    #[test]
    fn the_sink_says_which_containers_go_as_they_are_and_which_codec_leads() {
        let tv = sink(
            "http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_HP_HD_AAC,http-get:*:video/x-mkv:*,\
             rtsp-rtp-udp:*:video/mpeg:*, http-get:*:audio/mpeg:DLNA.ORG_PN=MP3,http-get:*:video/mp4:DLNA.ORG_PN=HEVC_MP4_MP_L51_AAC",
        );
        assert_eq!(tv[1], ("video/x-mkv".to_string(), "*".to_string()));
        assert_eq!(serde_json::to_string(&plays(&tv)).unwrap(), r#"["mkv","mp4"]"#);
        assert_eq!(decodes(&tv), Playback::Hevc);
        assert_eq!(decodes(&tv[..2]), Playback::H264, "no HEVC named");
        let everything = sink("http-get:*:*:*");
        assert_eq!(plays(&everything).iter().count(), 8);
        let silent = sink("");
        assert_eq!(serde_json::to_string(&plays(&silent)).unwrap(), r#"["mkv","mp4"]"#, "a TV that says nothing");
        let only_avi = sink("http-get:*:video/avi:*");
        assert_eq!(serde_json::to_string(&plays(&only_avi)).unwrap(), r#"["avi"]"#);
        let description = describe(LG, &Url::parse("http://192.168.1.40:1876/").unwrap(), TV).unwrap();
        let renderer = Renderer::new(description, "192.168.1.40".into(), tv);
        assert_eq!(renderer.mime(Container::Mkv), "video/x-mkv", "the TV's own spelling");
        assert_eq!(renderer.mime(Container::WebM), "video/webm");
    }

    #[test]
    fn an_mpeg_type_with_a_transport_stream_profile_takes_ts_files_in_the_tvs_spelling() {
        let tv = sink(
            "http-get:*:video/mpeg:DLNA.ORG_PN=AVC_TS_HD_24_AC3_T;DLNA.ORG_OP=01;DLNA.ORG_CI=0,\
             http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_HP_HD_AAC",
        );
        assert_eq!(serde_json::to_string(&plays(&tv)).unwrap(), r#"["mp4","ts"]"#);
        let description = describe(LG, &Url::parse("http://192.168.1.40:1876/").unwrap(), TV).unwrap();
        let renderer = Renderer::new(description, "192.168.1.40".into(), tv);
        assert_eq!(renderer.mime(Container::Ts), "video/mpeg", "the TV's own spelling");
        assert_eq!(renderer.mime(Container::Mp4), "video/mp4");
        let program = sink("http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_NTSC,http-get:*:video/mpeg:*");
        assert_eq!(serde_json::to_string(&plays(&program)).unwrap(), r#"["mpeg"]"#);
        let both =
            sink("http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_PAL,http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_TS_HD_NA");
        assert_eq!(serde_json::to_string(&plays(&both)).unwrap(), r#"["ts","mpeg"]"#);
    }

    #[test]
    fn times_read_and_write_as_avtransport_has_them() {
        assert_eq!(clock(0.0), "0:00:00");
        assert_eq!(clock(3_725.4), "1:02:05");
        assert_eq!(clock(-3.0), "0:00:00");
        assert_eq!(seconds("1:02:05"), Some(3_725.0));
        assert_eq!(seconds("0:00:07.500"), Some(7.5));
        assert_eq!(seconds("00:01:30"), Some(90.0));
        assert_eq!(seconds("NOT_IMPLEMENTED"), None);
        assert_eq!(seconds(""), None);
    }
}
