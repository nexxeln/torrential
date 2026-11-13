//! Playing on a TV: the TVs on the home network that play what they are sent, and the casts
//! playing on them. A TV here is any UPnP media renderer, as LG, Samsung, Sony, and most other
//! smart TVs are.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::catalog::VideoRef;
use crate::error::Error;
use crate::id::Target;
use crate::preferences::Playback;

/// A video container, which a TV takes or does not. Its codecs are another matter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Container {
    Mkv,
    Mp4,
    WebM,
    Avi,
    Ts,
    Mov,
    Mpeg,
    Wmv,
}

/// Each container's name, the extensions its files have, and the MIME types TVs list it by.
const CONTAINERS: [(Container, &str, &[&str], &[&str]); 8] = [
    (Container::Mkv, "mkv", &["mkv"], &["video/x-matroska", "video/x-mkv", "video/mkv", "video/matroska"]),
    (Container::Mp4, "mp4", &["mp4", "m4v"], &["video/mp4", "video/mpeg4", "video/x-m4v"]),
    (Container::WebM, "webm", &["webm"], &["video/webm"]),
    (Container::Avi, "avi", &["avi", "divx"], &["video/avi", "video/x-msvideo", "video/x-avi", "video/divx"]),
    (Container::Ts, "ts", &["ts", "m2ts", "mts"], &["video/mp2t", "video/vnd.dlna.mpeg-tts", "video/x-mpegts"]),
    (Container::Mov, "mov", &["mov"], &["video/quicktime"]),
    (Container::Mpeg, "mpeg", &["mpg", "mpeg"], &["video/mpeg"]),
    (Container::Wmv, "wmv", &["wmv"], &["video/x-ms-wmv"]),
];

impl Container {
    fn row(self) -> &'static (Container, &'static str, &'static [&'static str], &'static [&'static str]) {
        CONTAINERS.iter().find(|row| row.0 == self).expect("every container has a row")
    }

    pub fn name(self) -> &'static str {
        self.row().1
    }

    /// The MIME types a TV may list this container by, the usual one first.
    pub fn mimes(self) -> &'static [&'static str] {
        self.row().3
    }

    /// The container of a file named `path`, by its extension.
    pub fn of_path(path: &str) -> Option<Container> {
        let (_, extension) = path.rsplit_once('.')?;
        let extension = extension.to_ascii_lowercase();
        CONTAINERS.iter().find(|row| row.2.contains(&extension.as_str())).map(|row| row.0)
    }

    /// The container a MIME type names, in any case.
    pub fn of_mime(mime: &str) -> Option<Container> {
        let mime = mime.trim().to_ascii_lowercase();
        CONTAINERS.iter().find(|row| row.3.contains(&mime.as_str())).map(|row| row.0)
    }

    fn of_name(name: &str) -> Option<Container> {
        CONTAINERS.iter().find(|row| row.1 == name).map(|row| row.0)
    }

    fn bit(self) -> u16 {
        1 << CONTAINERS.iter().position(|row| row.0 == self).expect("every container has a row")
    }
}

/// The containers a TV takes. Written as their names, such as `["mkv", "mp4"]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Plays(u16);

impl Plays {
    pub fn contains(self, container: Container) -> bool {
        self.0 & container.bit() != 0
    }

    pub fn iter(self) -> impl Iterator<Item = Container> {
        CONTAINERS.iter().map(|row| row.0).filter(move |c| self.contains(*c))
    }

    /// Whether a file named `path` goes to the TV as it is.
    pub fn takes(self, path: &str) -> bool {
        Container::of_path(path).is_some_and(|container| self.contains(container))
    }
}

impl FromIterator<Container> for Plays {
    fn from_iter<I: IntoIterator<Item = Container>>(containers: I) -> Self {
        Plays(containers.into_iter().fold(0, |bits, container| bits | container.bit()))
    }
}

impl Serialize for Plays {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter().map(Container::name))
    }
}

impl<'de> Deserialize<'de> for Plays {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let names = Vec::<String>::deserialize(d)?;
        Ok(names.iter().filter_map(|name| Container::of_name(name)).collect())
    }
}

/// A TV, or another media renderer, that answered a search of the home network.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Tv {
    /// The renderer's UDN, such as `uuid:6a3e…`, which stays the same when the TV restarts.
    pub id: String,
    /// The name the TV gives itself, such as `[LG] webOS TV OLED55C1`.
    pub name: String,
    /// Its IP address on the home network.
    pub address: String,
    /// The containers a cast sends it as they are.
    pub plays: Plays,
    /// The newest video codec it says it decodes, which picks the sources a title cast tries.
    pub decodes: Playback,
    /// Whether torrential can set its volume.
    pub volume: bool,
}

/// What plays on a TV now.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CastState {
    /// Finding a source for the film or episode.
    Finding,
    /// Sent to the TV, which has not started playing it yet.
    Loading,
    Playing,
    Paused,
    /// Over. `error` says why when it did not end as asked or at the end of the video.
    Ended {
        error: Option<Error>,
    },
}

/// One cast on one TV, from the moment it is asked for until a minute after it ends.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Cast {
    /// The TV's ID.
    pub tv: String,
    pub tv_name: String,
    /// The film's name and year, the series' name and the episode, or the file's name.
    pub name: String,
    /// The film or episode, whose position the watch history keeps.
    pub video: Option<VideoRef>,
    /// The download it plays from, once a source starts.
    pub download: Option<String>,
    pub file: Option<u32>,
    /// The subtitle file sent with the video, relative to the torrent's folder.
    pub subtitle: Option<String>,
    #[serde(flatten)]
    pub state: CastState,
    /// Seconds, as the TV last said.
    pub position: Option<f64>,
    pub duration: Option<f64>,
    /// From 0 to 100, for a TV whose volume torrential can set.
    pub volume: Option<u8>,
    /// Unix seconds.
    pub started_at: i64,
}

impl Cast {
    pub fn ended(&self) -> bool {
        matches!(self.state, CastState::Ended { .. })
    }
}

/// The TVs found on the home network, and the casts playing or just ended.
#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct TvList {
    pub tvs: Vec<Tv>,
    pub casts: Vec<Cast>,
}

/// What a remote control asks a cast to do.
#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum CastAction {
    Pause,
    Resume,
    /// To `position` seconds into the video.
    Seek {
        position: f64,
    },
    Stop,
    /// From 0 to 100.
    Volume {
        level: u8,
    },
}

/// A cast to start.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct CastRequest {
    /// A TV's ID or a part of its name. `None` picks the only TV on the network.
    pub tv: Option<String>,
    /// A film or an episode, or a torrent.
    pub target: Target,
    /// For a title, an ID prefix of the one release to try.
    #[serde(default)]
    pub source: Option<String>,
    /// For a torrent, the index of the file to play. Without it, the main video plays.
    #[serde(default)]
    pub file: Option<u32>,
    /// For a torrent, the film or episode it holds, whose position the watch history keeps.
    #[serde(default)]
    pub video: Option<VideoRef>,
    /// The index of a subtitle file of the same torrent to show with the video.
    #[serde(default)]
    pub subtitle: Option<u32>,
    /// Seconds to start at, in place of where the person stopped.
    #[serde(default)]
    pub start_at: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_known_by_its_extension_and_a_tv_by_its_mime_types() {
        assert_eq!(Container::of_path("Sintel.2010.1080p.MKV"), Some(Container::Mkv));
        assert_eq!(Container::of_path("film.m4v"), Some(Container::Mp4));
        assert_eq!(Container::of_path("film.ogv"), None);
        assert_eq!(Container::of_path("README"), None);
        assert_eq!(Container::of_mime(" Video/X-MKV "), Some(Container::Mkv));
        assert_eq!(Container::of_mime("video/vnd.dlna.mpeg-tts"), Some(Container::Ts));
        assert_eq!(Container::of_mime("audio/mpeg"), None);
    }

    #[test]
    fn the_containers_a_tv_plays_read_and_write_as_their_names() {
        let plays: Plays = [Container::Mp4, Container::Mkv].into_iter().collect();
        assert!(plays.takes("film.mkv") && plays.takes("film.MP4") && !plays.takes("film.avi"));
        assert_eq!(serde_json::to_string(&plays).unwrap(), r#"["mkv","mp4"]"#);
        let read: Plays = serde_json::from_str(r#"["mp4","ogg","mkv"]"#).unwrap();
        assert_eq!(read, plays, "a name this version does not know is left out");
    }

    #[test]
    fn a_cast_reads_as_one_flat_document() {
        let cast = Cast {
            tv: "uuid:6a3e".into(),
            tv_name: "LG TV".into(),
            name: "Night of the Living Dead (1968)".into(),
            video: Some("tt0063350".parse().unwrap()),
            download: None,
            file: None,
            subtitle: None,
            state: CastState::Paused,
            position: Some(61.5),
            duration: Some(5760.0),
            volume: Some(12),
            started_at: 1_800_000_000,
        };
        let json = serde_json::to_value(&cast).unwrap();
        assert_eq!(json["state"], "paused");
        assert_eq!(json["video"], "tt0063350");
        assert_eq!(serde_json::from_value::<Cast>(json).unwrap(), cast);
        let seek = serde_json::to_value(CastAction::Seek { position: 90.0 }).unwrap();
        assert_eq!(seek, serde_json::json!({"action": "seek", "position": 90.0}));
    }
}
