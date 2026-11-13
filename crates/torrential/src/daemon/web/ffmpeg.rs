//! Finds ffmpeg and ffprobe, and builds the commands that turn a download into what a browser plays.

use std::path::{Path, PathBuf};

use super::media::Media;

/// Where ffmpeg lives when the engine's own `PATH` is the bare one launchd and systemd give a service.
const USUAL_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// Keeps the file's own timestamps in the fragments, so a stream that starts at 50 minutes says so.
/// `frag_discont` writes them, and `-copyts` with `-avoid_negative_ts disabled` keeps ffmpeg from
/// moving them to zero first.
const FRAGMENTED: &str = "frag_keyframe+empty_moov+default_base_moof+frag_discont";

/// Seconds between keyframes when the engine encodes the video, which is how far a seek can miss by.
const KEYFRAME_EVERY: f64 = 2.0;
const HIGHEST: (u32, u32) = (1920, 1080);

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Tools {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
}

/// ffmpeg and ffprobe, both found, which converting a file needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Programs {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Looks in `path`, the value of `PATH`, then in the usual folders.
    pub fn find(path: Option<&str>) -> Tools {
        let dirs: Vec<&Path> = path.unwrap_or_default().split(':').chain(USUAL_DIRS).map(Path::new).collect();
        Tools::find_in(&dirs)
    }

    fn find_in(dirs: &[&Path]) -> Tools {
        let find = |name: &str| dirs.iter().map(|dir| dir.join(name)).find(|path| path.is_absolute() && path.is_file());
        Tools { ffmpeg: find("ffmpeg"), ffprobe: find("ffprobe") }
    }

    pub fn both(&self) -> Option<Programs> {
        Some(Programs { ffmpeg: self.ffmpeg.clone()?, ffprobe: self.ffprobe.clone()? })
    }
}

/// How a person installs ffmpeg here, as the end of a sentence.
pub const INSTALL: &str =
    if cfg!(target_os = "macos") { "run brew install ffmpeg" } else { "install ffmpeg with your package manager" };

/// The video the stream carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Picture {
    /// The file's own, untouched.
    Copy,
    /// Encoded again as H.264, for a codec the browser cannot decode.
    H264,
}

/// The encoder to use for `Picture::H264`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoder {
    /// Apple's hardware encoder.
    VideoToolbox,
    X264,
}

impl Encoder {
    pub const fn of_host() -> Encoder {
        if cfg!(target_os = "macos") { Encoder::VideoToolbox } else { Encoder::X264 }
    }
}

pub fn probe(url: &str) -> Vec<String> {
    strings(&["-v", "error", "-print_format", "json", "-show_format", "-show_streams", "-show_chapters", url])
}

/// A JPEG of the frame at `t` seconds, 240 pixels wide, written to stdout. ffmpeg reads the file on
/// disk and not through the engine, so it cannot make the engine fetch a piece.
pub fn thumbnail(path: &Path, t: f64) -> Vec<String> {
    let input = format!("file:{}", path.display());
    let mut args = strings(&["-v", "error", "-nostdin", "-ss", &seconds(t), "-i", &input]);
    args.extend(strings(&["-frames:v", "1", "-an", "-sn", "-dn", "-vf", "scale=240:-2", "-q:v", "5"]));
    args.extend(strings(&["-f", "mjpeg", "pipe:1"]));
    args
}

/// A fragmented MP4 of the file at `url` from `t` seconds on, written to stdout: its video, and
/// `audio` as AAC.
pub fn stream(url: &str, media: &Media, t: f64, audio: Option<u32>, picture: Picture, encoder: Encoder) -> Vec<String> {
    let mut args = strings(&["-v", "error", "-nostdin"]);
    args.extend(from(t));
    args.extend(strings(&["-copyts", "-i", url]));
    let mut add = |more: &[&str]| args.extend(strings(more));
    if let Some(video) = &media.video {
        add(&["-map", &format!("0:{}", video.stream)]);
        match picture {
            Picture::Copy if video.codec == "hevc" => add(&["-c:v", "copy", "-tag:v", "hvc1"]),
            Picture::Copy => add(&["-c:v", "copy"]),
            Picture::H264 => {
                let (width, height) = HIGHEST;
                let fit = format!(
                    "scale=w='min({width},iw)':h='min({height},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p"
                );
                let between = (video.fps.unwrap_or(24.0) * KEYFRAME_EVERY).round().max(1.0);
                add(&["-vf", &fit, "-g", &format!("{between}")]);
                match encoder {
                    Encoder::VideoToolbox => {
                        add(&["-c:v", "h264_videotoolbox", "-profile:v", "high", "-b:v", bitrate(video.height)])
                    }
                    Encoder::X264 => {
                        add(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "21", "-profile:v", "high"])
                    }
                }
            }
        }
    }
    if let Some(track) = media.audio_track(audio) {
        add(&["-map", &format!("0:{}", track.stream)]);
        match track.codec.as_str() {
            "aac" => add(&["-c:a", "copy"]),
            _ => add(&["-c:a", "aac", "-ac", "2", "-b:a", "192k"]),
        }
    }
    // A chapter list would become a third track, which a Media Source buffer refuses.
    add(&["-map_chapters", "-1", "-map_metadata", "-1", "-avoid_negative_ts", "disabled"]);
    add(&["-movflags", FRAGMENTED, "-f", "mp4", "pipe:1"]);
    args
}

/// WebVTT of the subtitle track `stream` of the file at `url` from `t` seconds on, on the file's
/// own timeline.
pub fn embedded_subtitles(url: &str, stream: u32, t: f64) -> Vec<String> {
    let mut args = strings(&["-v", "error", "-nostdin"]);
    args.extend(from(t));
    args.extend(strings(&["-copyts", "-i", url]));
    args.extend(strings(&["-map", &format!("0:{stream}"), "-c:s", "webvtt", "-avoid_negative_ts", "disabled"]));
    args.extend(strings(&["-flush_packets", "1", "-f", "webvtt", "pipe:1"]));
    args
}

/// WebVTT of a subtitle file read from stdin. `format` is ffmpeg's name for what the file is, when
/// its name says.
pub fn subtitle_file(format: Option<&str>) -> Vec<String> {
    let mut args = strings(&["-v", "error"]);
    if let Some(format) = format {
        args.extend(strings(&["-f", format]));
    }
    args.extend(strings(&["-i", "pipe:0", "-c:s", "webvtt", "-f", "webvtt", "pipe:1"]));
    args
}

/// ffmpeg's name for the format of a subtitle file, by its extension.
pub fn subtitle_format(name: &str) -> Option<&'static str> {
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase();
    match extension.as_str() {
        "srt" => Some("srt"),
        "ass" | "ssa" => Some("ass"),
        "vtt" => Some("webvtt"),
        _ => None,
    }
}

/// The first line ffmpeg printed about a failure, without the address it puts in its prefix.
pub fn complaint(stderr: &str) -> String {
    let line = stderr.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("it printed no reason");
    let line = match (line.starts_with('['), line.split_once("] ")) {
        (true, Some((_, rest))) => rest,
        _ => line,
    };
    torrential_core::text::sanitize(line)
}

fn bitrate(height: u32) -> &'static str {
    match height {
        0..=480 => "2M",
        481..=720 => "4M",
        _ => "8M",
    }
}

/// The seek that starts a conversion `t` seconds in. A conversion from the start has none: a seek
/// reads the file's index, which a Matroska file keeps at its end, and playing from the beginning
/// would wait for the last piece of the download.
fn from(t: f64) -> Vec<String> {
    if t.is_finite() && t > 0.0 { strings(&["-ss", &seconds(t)]) } else { Vec::new() }
}

fn seconds(t: f64) -> String {
    format!("{:.3}", if t.is_finite() { t.max(0.0) } else { 0.0 })
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| arg.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::super::fetch::Readiness;
    use super::super::media::{Audio, Video};
    use super::*;

    const URL: &str = "http://127.0.0.1:47919/ab/0/film.mkv";

    fn media(codec: &str, audio: &[(&str, bool)]) -> Media {
        let audio = (1..)
            .zip(audio)
            .map(|(stream, &(codec, default))| Audio {
                stream,
                codec: codec.into(),
                channels: 6,
                language: None,
                title: None,
                default,
            })
            .collect();
        let video = Video {
            stream: 0,
            codec: codec.into(),
            mime: None,
            width: 3840,
            height: 2160,
            bit_depth: 10,
            hdr: false,
            fps: Some(23.976),
        };
        Media {
            duration: Some(60.0),
            container: "matroska".into(),
            video: Some(video),
            audio,
            subtitles: Vec::new(),
            direct: None,
            ffmpeg: Readiness::Ready,
            chapters: Vec::new(),
        }
    }

    #[test]
    fn a_stream_copies_what_browsers_decode_and_converts_the_rest() {
        let film = media("hevc", &[("ac3", false), ("aac", true)]);
        let line = |args: Vec<String>| args.join(" ");
        let tail = "-map_chapters -1 -map_metadata -1 -avoid_negative_ts disabled -movflags \
                    frag_keyframe+empty_moov+default_base_moof+frag_discont -f mp4 pipe:1";
        assert_eq!(
            line(stream(URL, &film, 3000.5, None, Picture::Copy, Encoder::X264)),
            format!(
                "-v error -nostdin -ss 3000.500 -copyts -i {URL} -map 0:0 -c:v copy -tag:v hvc1 -map 0:2 -c:a copy {tail}"
            )
        );
        assert_eq!(
            line(stream(URL, &media("h264", &[("ac3", false)]), 0.0, Some(1), Picture::Copy, Encoder::X264)),
            format!("-v error -nostdin -copyts -i {URL} -map 0:0 -c:v copy -map 0:1 -c:a aac -ac 2 -b:a 192k {tail}")
        );
        let fit = "-vf scale=w='min(1920,iw)':h='min(1080,ih)':force_original_aspect_ratio=decrease:\
                   force_divisible_by=2,format=yuv420p -g 48";
        assert_eq!(
            line(stream(URL, &media("mpeg4", &[]), -4.0, Some(7), Picture::H264, Encoder::VideoToolbox)),
            format!(
                "-v error -nostdin -copyts -i {URL} -map 0:0 {fit} -c:v h264_videotoolbox -profile:v high \
                 -b:v 8M {tail}"
            )
        );
        assert!(
            line(stream(URL, &film, 1.0, None, Picture::H264, Encoder::X264))
                .contains(&format!("{fit} -c:v libx264 -preset veryfast -crf 21 -profile:v high -map 0:2"))
        );
    }

    #[test]
    fn a_thumbnail_is_read_from_the_file_on_disk() {
        let args = thumbnail(Path::new("/downloads/Film (2024)/film.mkv"), 1230.0).join(" ");
        assert_eq!(
            args,
            "-v error -nostdin -ss 1230.000 -i file:/downloads/Film (2024)/film.mkv -frames:v 1 -an -sn -dn \
             -vf scale=240:-2 -q:v 5 -f mjpeg pipe:1"
        );
    }

    #[test]
    fn a_subtitle_files_name_says_its_format() {
        let formats = ["a.srt", "a.SSA", "a.en.vtt", "a.sub", "a"].map(subtitle_format);
        assert_eq!(formats, [Some("srt"), Some("ass"), Some("webvtt"), None, None]);
    }

    #[test]
    fn the_tools_are_found_in_the_first_folder_that_has_them() {
        let dir = tempfile::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        for (folder, name) in [(&first, "ffprobe"), (&second, "ffmpeg"), (&second, "ffprobe")] {
            std::fs::create_dir_all(folder).unwrap();
            std::fs::write(folder.join(name), "").unwrap();
        }
        let found = Tools::find_in(&[Path::new("relative"), &dir.path().join("none"), &first, &second]);
        assert_eq!(found, Tools { ffmpeg: Some(second.join("ffmpeg")), ffprobe: Some(first.join("ffprobe")) });
        assert!(found.both().is_some());
        assert_eq!(Tools::find_in(&[&first]).both(), None);
    }

    #[test]
    fn a_failure_reads_as_ffmpegs_first_line() {
        let stderr = "\n[matroska,webm @ 0x7a310a4300] EBML header parsing failed\nError opening input\n";
        assert_eq!(complaint(stderr), "EBML header parsing failed");
        assert_eq!(complaint("Error opening input file"), "Error opening input file");
        assert_eq!(complaint(" \n"), "it printed no reason");
    }
}
