//! What a file is by its extension: one row per extension, one column per question asked of it.
//! The columns disagree on a few rows, such as `ogv`, `divx` and `mts`, because each keeps the
//! answer its question had before they were one table.

/// What a file in a torrent is to a play.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Video,
    Subtitle,
}

/// One extension and what each part of torrential takes a file with it as.
#[derive(Debug)]
pub struct FileType {
    pub extension: &'static str,
    /// What a play picks the file as. A release name ending in a video extension drops it.
    pub kind: Option<Kind>,
    /// The container a TV takes it as.
    pub container: Option<Container>,
    /// The MIME type the engine serves it with.
    pub served_as: Option<&'static str>,
    /// The MIME type a browser plays it as without ffmpeg.
    pub browser: Option<&'static str>,
    /// ffmpeg's name for its format, where torrential names it to ffmpeg.
    pub ffmpeg: Option<&'static str>,
    /// Counts toward a torrent that is mostly media, where an executable file is a risk.
    pub media: bool,
}

const fn row(
    extension: &'static str,
    kind: Option<Kind>,
    container: Option<Container>,
    served_as: Option<&'static str>,
    browser: Option<&'static str>,
    ffmpeg: Option<&'static str>,
    media: bool,
) -> FileType {
    FileType { extension, kind, container, served_as, browser, ffmpeg, media }
}

const VIDEO: Option<Kind> = Some(Kind::Video);
const SUBTITLE: Option<Kind> = Some(Kind::Subtitle);
const MP4: Option<&str> = Some("video/mp4");
const WEBM: Option<&str> = Some("video/webm");

#[rustfmt::skip]
const TABLE: [FileType; 28] = [
    row("mkv",  VIDEO,    Some(Container::Mkv),  Some("video/x-matroska"),     None, Some("matroska"), true),
    row("mp4",  VIDEO,    Some(Container::Mp4),  MP4,                          MP4,  Some("mp4"),      true),
    row("m4v",  VIDEO,    Some(Container::Mp4),  MP4,                          MP4,  Some("mp4"),      true),
    row("mov",  VIDEO,    Some(Container::Mov),  None,                         MP4,  Some("mp4"),      true),
    row("webm", VIDEO,    Some(Container::WebM), WEBM,                         WEBM, Some("webm"),     true),
    row("avi",  VIDEO,    Some(Container::Avi),  None,                         None, None,             true),
    row("divx", None,     Some(Container::Avi),  None,                         None, None,             false),
    row("ts",   VIDEO,    Some(Container::Ts),   None,                         None, None,             true),
    row("m2ts", VIDEO,    Some(Container::Ts),   None,                         None, None,             true),
    row("mts",  None,     Some(Container::Ts),   None,                         None, None,             false),
    row("wmv",  VIDEO,    Some(Container::Wmv),  None,                         None, None,             true),
    row("mpg",  VIDEO,    Some(Container::Mpeg), None,                         None, None,             true),
    row("mpeg", VIDEO,    Some(Container::Mpeg), None,                         None, None,             true),
    row("ogv",  VIDEO,    None,                  None,                         None, None,             false),
    row("flv",  None,     None,                  None,                         None, None,             true),
    row("srt",  SUBTITLE, None,                  Some("application/x-subrip"), None, Some("srt"),      true),
    row("ass",  SUBTITLE, None,                  None,                         None, Some("ass"),      true),
    row("ssa",  SUBTITLE, None,                  None,                         None, Some("ass"),      false),
    row("vtt",  SUBTITLE, None,                  Some("text/vtt"),             None, Some("webvtt"),   false),
    row("sub",  SUBTITLE, None,                  None,                         None, None,             false),
    row("mp3",  None,     None,                  None,                         None, None,             true),
    row("flac", None,     None,                  None,                         None, None,             true),
    row("aac",  None,     None,                  None,                         None, None,             true),
    row("ogg",  None,     None,                  None,                         None, None,             true),
    row("opus", None,     None,                  None,                         None, None,             true),
    row("wav",  None,     None,                  None,                         None, None,             true),
    row("m4a",  None,     None,                  None,                         None, None,             true),
    row("wma",  None,     None,                  None,                         None, None,             true),
];

/// The extension of the file at `path`, in lower case: what follows the last dot of its name.
/// Empty when the name has no dot.
pub fn extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or("", |(_, extension)| extension).to_ascii_lowercase()
}

impl FileType {
    /// The row for the file at `path`, by its extension.
    pub fn of(path: &str) -> Option<&'static FileType> {
        Self::of_extension(&extension(path))
    }

    /// The row for `extension`, in any case.
    pub fn of_extension(extension: &str) -> Option<&'static FileType> {
        TABLE.iter().find(|row| row.extension.eq_ignore_ascii_case(extension))
    }
}

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

/// Each container's name and the MIME types TVs list it by. The extensions its files have are in
/// [`FileType`]'s table.
const CONTAINERS: [(Container, &str, &[&str]); 8] = [
    (Container::Mkv, "mkv", &["video/x-matroska", "video/x-mkv", "video/mkv", "video/matroska"]),
    (Container::Mp4, "mp4", &["video/mp4", "video/mpeg4", "video/x-m4v"]),
    (Container::WebM, "webm", &["video/webm"]),
    (Container::Avi, "avi", &["video/avi", "video/x-msvideo", "video/x-avi", "video/divx"]),
    (Container::Ts, "ts", &["video/mp2t", "video/vnd.dlna.mpeg-tts", "video/x-mpegts"]),
    (Container::Mov, "mov", &["video/quicktime"]),
    (Container::Mpeg, "mpeg", &["video/mpeg"]),
    (Container::Wmv, "wmv", &["video/x-ms-wmv"]),
];

impl Container {
    /// Every container, in the order a TV's list of them is written.
    pub const ALL: [Container; 8] = [
        Container::Mkv,
        Container::Mp4,
        Container::WebM,
        Container::Avi,
        Container::Ts,
        Container::Mov,
        Container::Mpeg,
        Container::Wmv,
    ];

    fn row(self) -> &'static (Container, &'static str, &'static [&'static str]) {
        CONTAINERS.iter().find(|row| row.0 == self).expect("every container has a row")
    }

    pub fn name(self) -> &'static str {
        self.row().1
    }

    /// The MIME types a TV may list this container by, the usual one first.
    pub fn mimes(self) -> &'static [&'static str] {
        self.row().2
    }

    /// The container of a file named `path`, by its extension.
    pub fn of_path(path: &str) -> Option<Container> {
        FileType::of(path)?.container
    }

    /// The container a MIME type names, in any case.
    pub fn of_mime(mime: &str) -> Option<Container> {
        let mime = mime.trim().to_ascii_lowercase();
        CONTAINERS.iter().find(|row| row.2.contains(&mime.as_str())).map(|row| row.0)
    }

    pub(crate) fn of_name(name: &str) -> Option<Container> {
        CONTAINERS.iter().find(|row| row.1 == name).map(|row| row.0)
    }

    pub(crate) fn bit(self) -> u16 {
        1 << CONTAINERS.iter().position(|row| row.0 == self).expect("every container has a row")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_extension_has_one_row_and_a_path_finds_it_by_its_last_dot() {
        let mut extensions: Vec<&str> = TABLE.iter().map(|row| row.extension).collect();
        extensions.sort_unstable();
        extensions.dedup();
        assert_eq!(extensions.len(), TABLE.len());
        assert_eq!(extension("Films/Night.of.the.Living.Dead.1968.MKV"), "mkv");
        assert_eq!(extension("Films.2010/README"), "");
        let kind = |path: &str| FileType::of(path).and_then(|row| row.kind);
        assert_eq!(
            [kind("a/film.ogv"), kind("a/film.Srt"), kind("a/film.divx"), kind("a/film.nfo")],
            [Some(Kind::Video), Some(Kind::Subtitle), None, None]
        );
    }
}
