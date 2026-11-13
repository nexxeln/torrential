use std::net::SocketAddr;

use super::upnp::{Description, Service};
use super::*;

/// A TV with the UDN `udn` that answered a search from `address`.
fn heard(udn: &str, address: &str) -> Known {
    let at = |path: &str| url::Url::parse(&format!("http://{address}:1876{path}")).unwrap();
    let description = Description {
        udn: udn.into(),
        name: "LG TV".into(),
        transport: Service { kind: "urn:schemas-upnp-org:service:AVTransport:1".into(), control: at("/AVTransport") },
        rendering: None,
        connection: None,
    };
    let renderer = Arc::new(Renderer::new(description, address.into(), Vec::new()));
    Known { renderer, location: at("/").to_string(), missed: 0 }
}

#[test]
fn a_device_that_copies_a_tvs_udn_from_another_address_is_listed_as_another_tv() {
    let casts = Casts::new(Weak::new(), Find::At(SocketAddr::from(([127, 0, 0, 1], 9))));
    let listed = |casts: &Casts| -> Vec<String> {
        casts.list().tvs.iter().map(|tv| format!("{} {} {}", tv.id, tv.name, tv.address)).collect()
    };
    casts.state().found(vec![heard("uuid:lg", "192.168.1.40")]);
    casts.state().found(vec![heard("uuid:lg", "192.168.1.40"), heard("uuid:lg", "192.168.1.66")]);
    assert_eq!(listed(&casts), ["uuid:lg@192.168.1.40 LG TV 192.168.1.40", "uuid:lg@192.168.1.66 LG TV 192.168.1.66"]);
    let error = casts.choose(Some("lg")).unwrap_err();
    assert_eq!(
        (error.code, error.message.as_str()),
        (ErrorCode::AmbiguousRef, "2 TVs answered: LG TV at 192.168.1.40, LG TV at 192.168.1.66")
    );
    assert_eq!(casts.choose(Some("192.168.1.66")).unwrap().tv.address, "192.168.1.66");
    assert_eq!(casts.choose(Some("uuid:lg@192.168.1.40")).unwrap().tv.address, "192.168.1.40");

    casts.state().found(vec![heard("uuid:lg", "192.168.1.41")]);
    assert_eq!(listed(&casts).len(), 3, "a TV at a new address is a new entry, and the old ones stay a search");
    casts.state().found(vec![heard("uuid:lg", "192.168.1.41")]);
    assert_eq!(listed(&casts), ["uuid:lg@192.168.1.41 LG TV 192.168.1.41"]);
}

#[test]
fn a_tv_is_named_by_its_id_its_address_or_any_part_of_its_name() {
    let (id, name) = ("uuid:6a3e0b2c@192.168.1.40", "[LG] webOS TV OLED55C1");
    assert!(names_tv("uuid:6a3e0b2c@192.168.1.40", id, name));
    assert!(names_tv("192.168.1.40", id, name));
    assert!(names_tv("lg", id, name));
    assert!(names_tv(" oled55 ", id, name));
    assert!(!names_tv("uuid:6a3e0b2c", id, name), "a UDN alone names any device that claims it");
    assert!(!names_tv("192.168.1.4", id, name), "an address names a TV whole");
    assert!(!names_tv("samsung", id, name));
    assert!(!names_tv("  ", id, name));
}

#[test]
fn a_tv_gets_the_dlna_headers_and_samsung_its_subtitle() {
    let video = Dlna {
        mime: "video/x-mkv".into(),
        caption: Some("http://192.168.1.23:47919/cast/9e1d/a.srt".into()),
        video: true,
    };
    assert_eq!(
        video.headers(),
        "transferMode.dlna.org: Streaming\r\n\
         contentFeatures.dlna.org: DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\r\n\
         CaptionInfo.sec: http://192.168.1.23:47919/cast/9e1d/a.srt\r\n"
    );
    let subtitle = Dlna { mime: "text/srt".into(), caption: None, video: false };
    assert_eq!(subtitle.headers(), "transferMode.dlna.org: Interactive\r\n");
}

#[test]
fn a_download_made_by_playing_one_film_or_episode_names_it() {
    let link = |span| TitleLink { title: "tt0063350".parse().unwrap(), span, release: None };
    assert_eq!(video_of(&link(Span::Film)), Some("tt0063350".parse().unwrap()));
    let episode = torrential_core::title::Episode { season: 3, episode: 5 };
    assert_eq!(video_of(&link(Span::Episode(episode))), Some("tt0063350:3:5".parse().unwrap()));
    assert_eq!(video_of(&link(Span::Season(3))), None, "a season pack holds many");
}

#[test]
fn a_tv_that_does_not_take_the_container_is_refused_in_a_sentence() {
    let tv = Tv {
        id: "uuid:6a3e".into(),
        name: "LG TV".into(),
        address: "192.168.1.40".into(),
        plays: [Container::Mkv, Container::Mp4].into_iter().collect(),
        decodes: torrential_core::Playback::H264,
        volume: true,
    };
    let error = refused(&tv, "Night/Night.avi");
    assert_eq!(error.message, "LG TV does not play AVI files, only MKV and MP4");
    assert_eq!(error.hint.as_deref(), Some("play it on this Mac, or choose a source in a format the TV plays"));
}

#[test]
fn the_list_keeps_thirty_two_tvs_and_drops_those_the_last_search_missed_first() {
    let casts = Casts::new(Weak::new(), Find::At(SocketAddr::from(([127, 0, 0, 1], 9))));
    let search = |first: u8| (first..first + 32).map(|n| heard(&format!("uuid:{n}"), &format!("10.0.0.{n}"))).collect();
    casts.state().found(search(1));
    casts.state().found(search(101));
    let hosts: Vec<u8> =
        casts.list().tvs.iter().map(|tv| tv.address.parse::<Ipv4Addr>().unwrap().octets()[3]).collect();
    assert_eq!(hosts.len(), 32);
    assert!(hosts.iter().all(|host| *host > 100), "{hosts:?}");
}

#[test]
fn a_cast_gives_up_on_a_tv_that_stops_answering_fails_every_poll_or_never_plays() {
    let start = Instant::now();
    let after = |seconds| start + Duration::from_secs(seconds);
    let why = |contact: &Contact, seconds| contact.lost(after(seconds), "LG TV").map(|error| error.message);

    let silent = Contact::new(start);
    assert_eq!(why(&silent, 14), None);
    assert_eq!(why(&silent, 15).as_deref(), Some("LG TV stopped answering"));

    let mut faulting = Contact { played: true, ..Contact::new(start) };
    faulting.answered = after(59);
    assert_eq!(why(&faulting, 59), None);
    faulting.answered = after(60);
    assert_eq!(why(&faulting, 60).as_deref(), Some("LG TV stopped saying what it plays"), "a fault is an answer");

    let mut loading = Contact::new(start);
    (loading.answered, loading.said) = (after(89), after(89));
    assert_eq!(why(&loading, 89), None);
    assert_eq!(why(&loading, 90).as_deref(), Some("LG TV took the video but did not start playing it"));
    loading.played = true;
    assert_eq!(why(&loading, 90), None);
}
