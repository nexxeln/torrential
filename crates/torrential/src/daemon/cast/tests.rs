use super::*;

#[test]
fn a_tv_is_named_by_its_id_or_any_part_of_its_name() {
    let (id, name) = ("uuid:6a3e0b2c", "[LG] webOS TV OLED55C1");
    assert!(names_tv("uuid:6a3e0b2c", id, name));
    assert!(names_tv("lg", id, name));
    assert!(names_tv(" oled55 ", id, name));
    assert!(!names_tv("uuid:6a3e", id, name), "an ID names a TV whole");
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
