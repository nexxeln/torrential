//! `torrential tv`: the TVs on the home network, playing on one, and the remote control.

use std::path::Path;
use std::time::Duration;

use torrential_core::protocol::Request;
use torrential_core::units::count;
use torrential_core::{Cast, CastAction, CastRequest, CastState, Error, ErrorCode, Target, Tv, TvList};

use super::connect::Conn;
use super::{emit, titles};
use crate::cli::{TvCommand, TvPlayArgs};
use crate::output::format;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};

pub fn run(mut conn: Conn, command: Option<TvCommand>, mode: Mode, cwd: &Path) -> Result<u8, Error> {
    let (tv, action) = match command {
        None | Some(TvCommand::List) => {
            let list: TvList = conn.ask(&Request::Tvs { fresh: true })?;
            return Ok(emit(mode, &list_doc(&list), &Typed { kind: DocKind::Tvs, body: &list }));
        }
        Some(TvCommand::Play(args)) => return play(conn, args, mode, cwd),
        Some(TvCommand::Pause(args)) => (args.tv, CastAction::Pause),
        Some(TvCommand::Resume(args)) => (args.tv, CastAction::Resume),
        Some(TvCommand::Seek { time, tv }) => (tv.tv, CastAction::Seek { position: time }),
        Some(TvCommand::Volume { level, tv }) => (tv.tv, CastAction::Volume { level }),
        Some(TvCommand::Stop(args)) => (args.tv, CastAction::Stop),
    };
    let cast: Box<Cast> = conn.ask(&Request::CastControl { tv, action })?;
    Ok(emit(mode, &cast_doc(&cast), &Typed { kind: DocKind::Cast, body: &*cast }))
}

fn play(mut conn: Conn, args: TvPlayArgs, mode: Mode, cwd: &Path) -> Result<u8, Error> {
    let TvPlayArgs { words, tv, source, file, subtitle, at } = args;
    let video = titles::named(&words)?;
    let target = match (&video, words.as_slice()) {
        (Some(video), _) => Target::Title(video.clone()),
        (None, [reference]) => Target::parse(reference, cwd)?,
        (None, _) => {
            return Err(Error::new(ErrorCode::Usage, format!("{} names no title or torrent", words.join(" ")))
                .with_hint("name a title ID such as tt0063350, or a download ID from torrential status"));
        }
    };
    match (&video, &source, file) {
        (None, Some(_), _) => return Err(titles::for_a_title("--source")),
        (Some(video), _, Some(_)) => return Err(torrential_core::target::not_a_torrent(video, "--file")),
        _ => {}
    }
    let request = Request::Cast(CastRequest { tv, target, source, file, video: None, subtitle, start_at: at });
    let cast: Box<Cast> = match &video {
        Some(video) => {
            let shown = titles::Shown::new(mode);
            titles::steps(&mut conn, &request, video, |step| shown.step(step, "Casting"))?
        }
        None => conn.ask(&request)?,
    };
    Ok(emit(mode, &cast_doc(&cast), &Typed { kind: DocKind::Cast, body: &*cast }))
}

fn list_doc(list: &TvList) -> Doc {
    let rows = list
        .tvs
        .iter()
        .map(|tv| {
            let cast = list.casts.iter().find(|cast| cast.tv == tv.id && !cast.ended());
            vec![
                Cell::styled(&tv.name, Token::Accent),
                Cell::new(&tv.address),
                Cell::new(plays(tv)),
                Cell::new(cast.map_or(String::new(), now)),
            ]
        })
        .collect();
    let mut summary = vec![count(list.tvs.len(), "TV")];
    if list.tvs.is_empty() {
        summary.push(
            "no TV answered: check that it is on, on the same network as the computer running torrential, and that its DLNA or media sharing setting is on"
                .to_string(),
        );
    }
    let next = (!list.tvs.is_empty()).then(|| "torrential tv play <title or download ID>".to_string());
    Doc { body: Body::Table { header: vec!["TV", "ADDRESS", "PLAYS", "NOW"], rows }, summary, next }
}

/// What a TV plays as they are, such as `MKV, MP4 (H.264)`.
fn plays(tv: &Tv) -> String {
    let containers: Vec<String> = tv.plays.iter().map(|c| c.name().to_ascii_uppercase()).collect();
    let codec = match tv.decodes {
        torrential_core::Playback::H264 => "H.264",
        torrential_core::Playback::Hevc | torrential_core::Playback::HevcHdr => "HEVC",
    };
    format!("{} ({codec})", containers.join(", "))
}

/// A cast in a few words, such as `playing Sintel 4:05 / 14:48`.
fn now(cast: &Cast) -> String {
    let verb = match &cast.state {
        CastState::Finding => "finding a source for",
        CastState::Loading => "starting",
        CastState::Playing => "playing",
        CastState::Paused => "paused",
        CastState::Ended { .. } => "ended",
    };
    format!("{verb} {}{}", cast.name, times(cast).map_or(String::new(), |t| format!(" {t}")))
}

/// `4:05 / 14:48`, or `4:05` when the TV does not say how long the video is.
fn times(cast: &Cast) -> Option<String> {
    let clock = |seconds: f64| format::clock(Duration::from_secs_f64(seconds.max(0.0)));
    let position = clock(cast.position?);
    Some(cast.duration.map_or(position.clone(), |duration| format!("{position} / {}", clock(duration))))
}

fn cast_doc(cast: &Cast) -> Doc {
    let state = match &cast.state {
        CastState::Finding => "finding a source",
        CastState::Loading => "starting",
        CastState::Playing => "playing",
        CastState::Paused => "paused",
        CastState::Ended { error: None } => "ended",
        CastState::Ended { error: Some(_) } => "failed",
    };
    let mut fields = vec![
        ("tv", Cell::styled(&cast.tv_name, Token::Accent)),
        ("video", Cell::new(&cast.name)),
        ("state", Cell::styled(state, if cast.ended() { Token::Muted } else { Token::Mint })),
    ];
    if let Some(times) = times(cast) {
        fields.push(("at", Cell::new(times)));
    }
    if let Some(volume) = cast.volume {
        fields.push(("volume", Cell::new(volume.to_string())));
    }
    if let Some(subtitle) = &cast.subtitle {
        fields.push(("subtitle", Cell::new(subtitle)));
    }
    if let (Some(download), Some(file)) = (&cast.download, cast.file) {
        fields.push((
            "download",
            Cell::new(format!("{} file {file}", &download[..download.len().min(torrential_core::id::SHORT_ID)])),
        ));
    }
    let summary = match &cast.state {
        CastState::Ended { error: Some(error) } => vec![error.message.clone()],
        _ => Vec::new(),
    };
    let next = match &cast.state {
        CastState::Playing => Some("torrential tv pause"),
        CastState::Paused => Some("torrential tv resume"),
        CastState::Finding | CastState::Loading => Some("torrential tv"),
        CastState::Ended { .. } => None,
    };
    Doc { body: Body::Fields { title: Vec::new(), fields }, summary, next: next.map(String::from) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::{Container, Playback};

    fn tv(name: &str, decodes: Playback) -> Tv {
        Tv {
            id: format!("uuid:{name}"),
            name: name.into(),
            address: "192.168.1.40".into(),
            plays: [Container::Mkv, Container::Mp4].into_iter().collect(),
            decodes,
            volume: true,
        }
    }

    fn sintel(state: CastState) -> Cast {
        Cast {
            tv: "uuid:[LG] webOS TV OLED55C1".into(),
            tv_name: "[LG] webOS TV OLED55C1".into(),
            name: "Sintel (2010)".into(),
            video: Some("tt1727587".parse().unwrap()),
            download: Some("6bc1af0e9d1c55a2d8e1c3f0a9b8c7d6e5f40312".into()),
            file: Some(0),
            subtitle: Some("Sintel/Sintel.en.srt".into()),
            state,
            position: Some(245.0),
            duration: Some(888.0),
            volume: Some(12),
            started_at: 1_800_000_000,
        }
    }

    #[test]
    fn the_list_shows_each_tv_what_it_plays_and_what_plays_on_it() {
        let list = TvList {
            tvs: vec![tv("[LG] webOS TV OLED55C1", Playback::Hevc), tv("Bedroom TV", Playback::H264)],
            casts: vec![sintel(CastState::Playing)],
        };
        assert_eq!(
            list_doc(&list).plain(),
            "TV\tADDRESS\tPLAYS\tNOW\n\
             [LG] webOS TV OLED55C1\t192.168.1.40\tMKV, MP4 (HEVC)\tplaying Sintel (2010) 4:05 / 14:48\n\
             Bedroom TV\t192.168.1.40\tMKV, MP4 (H.264)\t\n\
             # 2 TVs\n\
             next: torrential tv play <title or download ID>\n"
        );
        assert_eq!(
            list_doc(&TvList::default()).plain(),
            "TV\tADDRESS\tPLAYS\tNOW\n# 0 TVs\n\
             # no TV answered: check that it is on, on the same network as the computer running torrential, and that its DLNA or media sharing setting is on\n"
        );
    }

    #[test]
    fn a_cast_shows_the_tv_the_video_and_where_it_is() {
        assert_eq!(
            cast_doc(&sintel(CastState::Paused)).plain(),
            "tv:       [LG] webOS TV OLED55C1\n\
             video:    Sintel (2010)\n\
             state:    paused\n\
             at:       4:05 / 14:48\n\
             volume:   12\n\
             subtitle: Sintel/Sintel.en.srt\n\
             download: 6bc1af file 0\n\
             next:     torrential tv resume\n"
        );
        let lost = Error::new(ErrorCode::NetworkUnavailable, "[LG] webOS TV OLED55C1 stopped answering");
        let failed =
            Cast { position: None, volume: None, subtitle: None, ..sintel(CastState::Ended { error: Some(lost) }) };
        assert_eq!(
            cast_doc(&failed).plain(),
            "tv:       [LG] webOS TV OLED55C1\n\
             video:    Sintel (2010)\n\
             state:    failed\n\
             download: 6bc1af file 0\n\
             # [LG] webOS TV OLED55C1 stopped answering\n"
        );
    }
}
