//! What is inside a video file, as ffprobe reports it and as the web player needs it.

use serde::{Deserialize, Serialize};
use torrential_core::{Error, ErrorCode};

use super::fetch::Readiness;

/// The JSON of `GET /api/media`.
#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct Media {
    /// Seconds.
    pub duration: Option<f64>,
    pub container: String,
    pub video: Option<Video>,
    pub audio: Vec<Audio>,
    pub subtitles: Vec<Subtitle>,
    /// A type for `canPlayType`, when browsers read the file's container.
    pub direct: Option<String>,
    /// Whether the engine can convert the file as it plays.
    pub ffmpeg: Readiness,
    /// The file's own chapters, in order.
    pub chapters: Vec<Chapter>,
}

#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct Chapter {
    /// Seconds.
    pub start: f64,
    pub end: f64,
    pub title: Option<String>,
}

#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct Video {
    pub stream: u32,
    pub codec: String,
    /// The RFC 6381 codec string. `None` when MP4 cannot carry the codec.
    pub mime: Option<String>,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub hdr: bool,
    pub fps: Option<f64>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct Audio {
    pub stream: u32,
    pub codec: String,
    pub channels: u32,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct Subtitle {
    pub stream: u32,
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    /// False for picture subtitles, which cannot become WebVTT.
    pub text: bool,
}

/// Subtitles stored as pictures.
const PICTURE_SUBTITLES: [&str; 5] = ["hdmv_pgs_subtitle", "dvd_subtitle", "dvb_subtitle", "xsub", "dvb_teletext"];
const HDR_TRANSFERS: [&str; 2] = ["smpte2084", "arib-std-b67"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct Probe {
    streams: Vec<Stream>,
    format: Format,
    chapters: Vec<Marked>,
}

/// A chapter as ffprobe prints it.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Marked {
    start_time: Option<String>,
    end_time: Option<String>,
    tags: Tags,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Format {
    format_name: String,
    duration: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Stream {
    index: u32,
    codec_name: String,
    codec_type: String,
    profile: Option<String>,
    mime_codec_string: Option<String>,
    width: u32,
    height: u32,
    pix_fmt: Option<String>,
    level: Option<i32>,
    color_transfer: Option<String>,
    avg_frame_rate: Option<String>,
    channels: u32,
    disposition: Disposition,
    tags: Tags,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Disposition {
    default: u8,
    forced: u8,
    attached_pic: u8,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Tags {
    #[serde(alias = "LANGUAGE")]
    language: Option<String>,
    #[serde(alias = "TITLE")]
    title: Option<String>,
}

impl Media {
    /// Reads the JSON `ffprobe -show_format -show_streams` printed about the file named `name`.
    pub fn from_ffprobe(json: &[u8], name: &str) -> Result<Media, Error> {
        let probe: Probe = serde_json::from_slice(json)
            .map_err(|e| Error::new(ErrorCode::Internal, format!("ffprobe printed JSON that does not parse: {e}")))?;
        let of = |kind: &'static str| probe.streams.iter().filter(move |s| s.codec_type == kind);
        let text = |value: &Option<String>| value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(String::from);
        let language = |s: &Stream| text(&s.tags.language).filter(|language| language != "und");
        let video = of("video").find(|s| s.disposition.attached_pic == 0).map(|s| {
            let bit_depth = bit_depth(s.pix_fmt.as_deref().unwrap_or_default());
            Video {
                stream: s.index,
                codec: s.codec_name.clone(),
                mime: video_mime(s, bit_depth),
                width: s.width,
                height: s.height,
                bit_depth,
                hdr: s.color_transfer.as_deref().is_some_and(|transfer| HDR_TRANSFERS.contains(&transfer)),
                fps: s.avg_frame_rate.as_deref().and_then(rate),
            }
        });
        let audio: Vec<Audio> = of("audio")
            .map(|s| Audio {
                stream: s.index,
                codec: s.codec_name.clone(),
                channels: s.channels,
                language: language(s),
                title: text(&s.tags.title),
                default: s.disposition.default == 1,
            })
            .collect();
        let subtitles = of("subtitle")
            .map(|s| Subtitle {
                stream: s.index,
                codec: s.codec_name.clone(),
                language: language(s),
                title: text(&s.tags.title),
                default: s.disposition.default == 1,
                forced: s.disposition.forced == 1,
                text: !PICTURE_SUBTITLES.contains(&s.codec_name.as_str()),
            })
            .collect();
        let container = container(&probe.format.format_name, name, video.as_ref(), &audio);
        let direct = direct(&container, video.as_ref(), audio.first());
        let duration = probe.format.duration.and_then(|d| d.parse::<f64>().ok()).filter(|d| *d > 0.0);
        let seconds = |time: &Option<String>| time.as_deref().and_then(|time| time.parse::<f64>().ok());
        let mut chapters: Vec<Chapter> = probe
            .chapters
            .iter()
            .filter_map(|c| {
                Some(Chapter { start: seconds(&c.start_time)?, end: seconds(&c.end_time)?, title: text(&c.tags.title) })
            })
            .filter(|chapter| chapter.end > chapter.start)
            .collect();
        chapters.sort_by(|a, b| a.start.total_cmp(&b.start));
        Ok(Media { duration, container, video, audio, subtitles, direct, ffmpeg: Readiness::Ready, chapters })
    }

    /// What the file's name and header say when ffprobe is not installed.
    pub fn without_ffprobe(name: &str, duration: Option<f64>, ffmpeg: Readiness) -> Media {
        let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase();
        let (container, direct) = match extension.as_str() {
            "mp4" | "m4v" | "mov" => ("mp4", Some("video/mp4")),
            "webm" => ("webm", Some("video/webm")),
            "mkv" => ("matroska", None),
            other => (other, None),
        };
        Media {
            duration,
            container: container.to_string(),
            video: None,
            audio: Vec::new(),
            subtitles: Vec::new(),
            direct: direct.map(String::from),
            ffmpeg,
            chapters: Vec::new(),
        }
    }

    /// The audio track with this stream index, or the one a player starts with: the first marked
    /// as the default, or else the first.
    pub fn audio_track(&self, stream: Option<u32>) -> Option<&Audio> {
        match stream {
            Some(stream) => self.audio.iter().find(|a| a.stream == stream),
            None => self.audio.iter().find(|a| a.default).or(self.audio.first()),
        }
    }
}

/// `matroska` and `webm` are one format to ffprobe. A file is WebM by its name or by holding only
/// what WebM allows.
fn container(format: &str, name: &str, video: Option<&Video>, audio: &[Audio]) -> String {
    let first = format.split(',').next().unwrap_or_default();
    let webm = || {
        name.to_ascii_lowercase().ends_with(".webm")
            || (video.is_some_and(|v| matches!(v.codec.as_str(), "vp8" | "vp9" | "av1"))
                && audio.iter().all(|a| matches!(a.codec.as_str(), "opus" | "vorbis")))
    };
    match first {
        "mov" => "mp4".to_string(),
        "matroska" if webm() => "webm".to_string(),
        other => other.to_string(),
    }
}

fn direct(container: &str, video: Option<&Video>, audio: Option<&Audio>) -> Option<String> {
    let video = video?;
    let (kind, picture, sound) = match container {
        "mp4" => {
            let sound = audio.map(|a| match a.codec.as_str() {
                "aac" => "mp4a.40.2",
                "ac3" => "ac-3",
                "eac3" => "ec-3",
                "mp3" => "mp4a.6B",
                "dts" => "dtsc",
                "truehd" => "mlpa",
                other => other,
            });
            ("video/mp4", video.mime.clone()?, sound)
        }
        "webm" => {
            let picture = match video.codec.as_str() {
                "vp8" | "vp9" => video.codec.clone(),
                _ => video.mime.clone()?,
            };
            ("video/webm", picture, audio.map(|a| a.codec.as_str()))
        }
        _ => return None,
    };
    Some(match sound {
        Some(sound) => format!("{kind}; codecs=\"{picture}, {sound}\""),
        None => format!("{kind}; codecs=\"{picture}\""),
    })
}

/// The codec string of a video stream inside MP4. ffprobe gives one for some codecs.
fn video_mime(s: &Stream, bit_depth: u8) -> Option<String> {
    let profile = s.profile.as_deref().unwrap_or_default();
    let level = s.level.filter(|level| *level > 0);
    let given = s.mime_codec_string.as_deref().filter(|mime| !mime.is_empty());
    match s.codec_name.as_str() {
        "hevc" => {
            let built = || {
                let (profile, compatibility) = match profile {
                    "Main" => (1, 6),
                    "Main 10" => (2, 4),
                    _ => (4, 10),
                };
                format!("hvc1.{profile}.{compatibility}.L{}.B0", level.unwrap_or(120))
            };
            // The engine tags HEVC `hvc1` when it converts, which Safari needs.
            Some(given.map_or_else(built, |mime| mime.replacen("hev1", "hvc1", 1)))
        }
        "vp8" => None,
        _ if given.is_some() => given.map(String::from),
        "h264" => {
            let profile = match profile {
                "Baseline" | "Constrained Baseline" => 0x42,
                "Main" => 0x4D,
                "Extended" => 0x58,
                "High 10" | "High 10 Intra" => 0x6E,
                "High 4:2:2" | "High 4:2:2 Intra" => 0x7A,
                "High 4:4:4 Predictive" | "High 4:4:4 Intra" => 0xF4,
                _ => 0x64,
            };
            Some(format!("avc1.{profile:02x}00{:02x}", level.unwrap_or(40)))
        }
        "av1" => {
            let profile = match profile {
                "High" => 1,
                "Professional" => 2,
                _ => 0,
            };
            Some(format!("av01.{profile}.{:02}M.{bit_depth:02}", level.unwrap_or(8)))
        }
        "vp9" => {
            let profile = profile.strip_prefix("Profile ").and_then(|n| n.parse::<u8>().ok()).unwrap_or(0);
            Some(format!("vp09.{profile:02}.{:02}.{bit_depth:02}", level.unwrap_or(40)))
        }
        _ => None,
    }
}

/// Bits per color sample, read from a pixel format such as `yuv420p10le`.
fn bit_depth(pix_fmt: &str) -> u8 {
    let format = pix_fmt.trim_end_matches("le").trim_end_matches("be");
    [16, 14, 12, 10].into_iter().find(|bits| format.ends_with(&bits.to_string())).unwrap_or(8)
}

/// A rate such as `24000/1001` as frames per second.
fn rate(text: &str) -> Option<f64> {
    let (frames, seconds) = text.split_once('/')?;
    let (frames, seconds) = (frames.parse::<f64>().ok()?, seconds.parse::<f64>().ok()?);
    (frames > 0.0 && seconds > 0.0).then(|| (frames / seconds * 1000.0).round() / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffprobe 9.0's report of a Matroska film: HEVC Main 10 in HDR, E-AC-3 and AAC audio, a cover
    /// picture, text and picture subtitles, and chapters out of order.
    const FILM: &str = r#"{"streams":[
        {"index":0,"codec_name":"hevc","profile":"Main 10","codec_type":"video","width":3840,"height":2160,
         "pix_fmt":"yuv420p10le","level":153,"color_transfer":"smpte2084","avg_frame_rate":"24000/1001",
         "disposition":{"default":1,"forced":0,"attached_pic":0}},
        {"index":1,"codec_name":"eac3","codec_type":"audio","mime_codec_string":"ec-3","channels":6,
         "avg_frame_rate":"0/0","disposition":{"default":1,"forced":0,"attached_pic":0},
         "tags":{"language":"eng","title":"Surround 5.1"}},
        {"index":2,"codec_name":"aac","codec_type":"audio","mime_codec_string":"mp4a.40.2","channels":2,
         "disposition":{"default":0,"forced":0,"attached_pic":0},"tags":{"LANGUAGE":"fra","title":" "}},
        {"index":3,"codec_name":"subrip","codec_type":"subtitle",
         "disposition":{"default":0,"forced":1,"attached_pic":0},"tags":{"language":"eng","title":"Forced"}},
        {"index":4,"codec_name":"hdmv_pgs_subtitle","codec_type":"subtitle",
         "disposition":{"default":0,"forced":0,"attached_pic":0},"tags":{"language":"und"}},
        {"index":5,"codec_name":"mjpeg","codec_type":"video","width":600,"height":900,
         "disposition":{"default":0,"forced":0,"attached_pic":1}}
      ],"chapters":[
        {"id":2,"time_base":"1/1000000000","start_time":"40.000000","end_time":"90.000000","tags":{"title":"The Middle"}},
        {"id":1,"time_base":"1/1000000000","start_time":"0.000000","end_time":"40.000000","tags":{"title":"Opening"}},
        {"id":3,"time_base":"1/1000000000","start_time":"90.000000","end_time":"5400.250000"},
        {"id":4,"time_base":"1/1000000000","start_time":"100.000000","end_time":"100.000000","tags":{"title":"Empty"}}
      ],"format":{"format_name":"matroska,webm","start_time":"0.000000","duration":"5400.250000"}}"#;

    /// An MP4 as ffprobe reports it, cut to the fields `Media` reads.
    fn mp4(video: &str, audio: &str) -> String {
        format!(
            r#"{{"streams":[{{"index":0,"codec_type":"video","width":1920,"height":1080,"avg_frame_rate":"25/1",
                {video}}},{{"index":1,"codec_type":"audio","channels":2,{audio}}}],
                "format":{{"format_name":"mov,mp4,m4a,3gp,3g2,mj2","duration":"60.5"}}}}"#
        )
    }

    #[test]
    fn a_film_lists_its_tracks_and_cannot_play_as_it_is() {
        let media = Media::from_ffprobe(FILM.as_bytes(), "Film.2160p.mkv").unwrap();
        insta::assert_json_snapshot!(media, @r#"
        {
          "duration": 5400.25,
          "container": "matroska",
          "video": {
            "stream": 0,
            "codec": "hevc",
            "mime": "hvc1.2.4.L153.B0",
            "width": 3840,
            "height": 2160,
            "bit_depth": 10,
            "hdr": true,
            "fps": 23.976
          },
          "audio": [
            {
              "stream": 1,
              "codec": "eac3",
              "channels": 6,
              "language": "eng",
              "title": "Surround 5.1",
              "default": true
            },
            {
              "stream": 2,
              "codec": "aac",
              "channels": 2,
              "language": "fra",
              "title": null,
              "default": false
            }
          ],
          "subtitles": [
            {
              "stream": 3,
              "codec": "subrip",
              "language": "eng",
              "title": "Forced",
              "default": false,
              "forced": true,
              "text": true
            },
            {
              "stream": 4,
              "codec": "hdmv_pgs_subtitle",
              "language": null,
              "title": null,
              "default": false,
              "forced": false,
              "text": false
            }
          ],
          "direct": null,
          "ffmpeg": "ready",
          "chapters": [
            {
              "start": 0.0,
              "end": 40.0,
              "title": "Opening"
            },
            {
              "start": 40.0,
              "end": 90.0,
              "title": "The Middle"
            },
            {
              "start": 90.0,
              "end": 5400.25,
              "title": null
            }
          ]
        }
        "#);
        assert_eq!(media.audio_track(None).map(|a| a.stream), Some(1));
        assert_eq!(media.audio_track(Some(2)).map(|a| a.stream), Some(2));
        assert_eq!(media.audio_track(Some(3)), None);
    }

    #[test]
    fn each_codec_gets_the_string_browsers_know_it_by() {
        let mime = |fields: &str| {
            let json = mp4(fields, r#""codec_name":"aac""#);
            Media::from_ffprobe(json.as_bytes(), "a.mp4").unwrap().video.unwrap().mime
        };
        let read = [
            mime(r#""codec_name":"h264","profile":"High","level":40"#),
            mime(r#""codec_name":"h264","profile":"Main","level":31"#),
            mime(r#""codec_name":"h264","profile":"Constrained Baseline","level":30"#),
            mime(r#""codec_name":"h264","profile":"High 10","level":51,"pix_fmt":"yuv420p10le""#),
            mime(r#""codec_name":"h264","profile":"High","level":30,"mime_codec_string":"avc1.64001e""#),
            mime(r#""codec_name":"hevc","profile":"Main","level":120"#),
            mime(r#""codec_name":"hevc","profile":"Main 10","level":150,"pix_fmt":"yuv420p10le""#),
            mime(r#""codec_name":"hevc","profile":"Main","level":93,"mime_codec_string":"hev1.1.6.L93.B0""#),
            mime(r#""codec_name":"av1","profile":"Main","level":8,"pix_fmt":"yuv420p10le""#),
            mime(r#""codec_name":"vp9","profile":"Profile 0","level":-99"#),
            mime(r#""codec_name":"vp9","profile":"Profile 2","level":41,"pix_fmt":"yuv420p10le""#),
            mime(r#""codec_name":"mpeg4","profile":"Advanced Simple Profile","level":5"#),
        ];
        let expected = [
            Some("avc1.640028"),
            Some("avc1.4d001f"),
            Some("avc1.42001e"),
            Some("avc1.6e0033"),
            Some("avc1.64001e"),
            Some("hvc1.1.6.L120.B0"),
            Some("hvc1.2.4.L150.B0"),
            Some("hvc1.1.6.L93.B0"),
            Some("av01.0.08M.10"),
            Some("vp09.00.40.08"),
            Some("vp09.02.41.10"),
            None,
        ];
        assert_eq!(read, expected.map(|mime| mime.map(String::from)));
    }

    #[test]
    fn a_container_browsers_read_names_the_type_to_try() {
        let direct = |name: &str, json: String| Media::from_ffprobe(json.as_bytes(), name).unwrap().direct;
        let h264 = r#""codec_name":"h264","profile":"High","level":40"#;
        let webm = |video: &str, audio: &str| {
            mp4(&format!(r#""codec_name":"{video}""#), &format!(r#""codec_name":"{audio}""#))
                .replace("mov,mp4,m4a,3gp,3g2,mj2", "matroska,webm")
        };
        let read = [
            direct("a.mp4", mp4(h264, r#""codec_name":"aac""#)),
            direct("a.mp4", mp4(h264, r#""codec_name":"ac3""#)),
            direct("a.mp4", mp4(h264, r#""codec_name":"dts""#)),
            direct("a.mp4", mp4(r#""codec_name":"mpeg4""#, r#""codec_name":"mp3""#)),
            direct("a.webm", webm("vp9", "opus")),
            direct("a.mkv", webm("vp8", "vorbis")),
            direct("a.mkv", webm("vp9", "aac")),
            direct("a.mkv", webm("h264", "aac")),
        ];
        let expected = [
            Some(r#"video/mp4; codecs="avc1.640028, mp4a.40.2""#),
            Some(r#"video/mp4; codecs="avc1.640028, ac-3""#),
            Some(r#"video/mp4; codecs="avc1.640028, dtsc""#),
            None,
            Some(r#"video/webm; codecs="vp9, opus""#),
            Some(r#"video/webm; codecs="vp8, vorbis""#),
            None,
            None,
        ];
        assert_eq!(read, expected.map(|direct| direct.map(String::from)));
    }

    #[test]
    fn a_file_without_ffprobe_goes_by_its_name() {
        let read = |name| {
            let media = Media::without_ffprobe(name, Some(7.0), Readiness::Fetch);
            (media.container, media.direct, media.ffmpeg, media.duration)
        };
        let fetch = Readiness::Fetch;
        assert_eq!(read("Film.MP4"), ("mp4".to_string(), Some("video/mp4".to_string()), fetch, Some(7.0)));
        assert_eq!(read("clip.webm"), ("webm".to_string(), Some("video/webm".to_string()), fetch, Some(7.0)));
        assert_eq!(read("Film.mkv"), ("matroska".to_string(), None, fetch, Some(7.0)));
        assert_eq!(read("Film.avi"), ("avi".to_string(), None, fetch, Some(7.0)));
    }

    #[test]
    fn output_that_is_not_json_is_an_error() {
        assert_eq!(Media::from_ffprobe(b"moov atom not found", "a.mp4").unwrap_err().code, ErrorCode::Internal);
    }
}
