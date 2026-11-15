//! What a person names: text read as a torrent ID, an ID prefix, a link, a `.torrent` file, or a
//! film or episode.

use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};

use crate::catalog::VideoRef;
use crate::error::{Error, ErrorCode};
use crate::id::{HexPrefix, InfoHash, TorrentId, decode_hex, parse_full_infohash};

impl FromStr for TorrentId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        decode_hex::<20>(s).map(Self::from_bytes).ok_or_else(|| invalid_ref(s, "a torrent ID is 40 hex characters"))
    }
}

impl<'de> Deserialize<'de> for TorrentId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl FromStr for HexPrefix {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let lower = s.to_ascii_lowercase();
        let (locator, hex) = match lower.strip_prefix('u') {
            Some(rest) => (true, rest.to_string()),
            None => (false, lower),
        };
        let valid = (Self::MIN_LEN..=40).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if valid {
            Ok(Self { locator, hex })
        } else {
            Err(invalid_ref(s, "an ID is 4 to 40 hex characters, with a leading u for a result that has no infohash"))
        }
    }
}

/// What a user named on the command line, parsed once at the boundary.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Target {
    /// A prefix of a torrent ID or locator ID, resolved against downloads and recent results.
    Prefix(String),
    /// A full infohash. A recent search result with this ID lends its links; otherwise peers
    /// come from the DHT.
    InfoHash(InfoHash),
    Magnet(String),
    /// An `http` or `https` URL that serves a `.torrent` file.
    TorrentUrl(String),
    /// An absolute path to a `.torrent` file.
    TorrentFile(PathBuf),
    /// A film or an episode, such as `tt0063350` or `tt0903747:3:5`. A title ID is never an ID
    /// prefix, since `t` is not hex.
    Title(VideoRef),
}

impl Target {
    /// Parses user input in a fixed order: magnet link, URL, full infohash, `.torrent`
    /// path, title, ID prefix, then any other existing file. `cwd` resolves relative paths.
    pub fn parse(input: &str, cwd: &std::path::Path) -> Result<Self, Error> {
        let input = input.trim();
        if input.get(..8).is_some_and(|scheme| scheme.eq_ignore_ascii_case("magnet:?")) {
            return Ok(Self::Magnet(input.to_string()));
        }
        let lower = input.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Ok(Self::TorrentUrl(input.to_string()));
        }
        if let Some(hash) = parse_full_infohash(input) {
            return Ok(Self::InfoHash(hash));
        }
        let path = cwd.join(input);
        if lower.ends_with(".torrent") {
            return if path.is_file() {
                Ok(Self::TorrentFile(path))
            } else {
                Err(Error::new(ErrorCode::InvalidRef, format!("no such file: {}", path.display())))
            };
        }
        if let Ok(video) = input.parse::<VideoRef>() {
            return Ok(Self::Title(video));
        }
        match input.parse::<HexPrefix>() {
            Ok(prefix) => Ok(Self::Prefix(if prefix.locator { format!("u{}", prefix.hex) } else { prefix.hex })),
            Err(_) if path.is_file() => Ok(Self::TorrentFile(path)),
            Err(err) => Err(err),
        }
    }
}

/// The error for a command or a flag that takes a torrent and was given a title. Only `play` and
/// `get` take a title.
pub fn not_a_torrent(video: &VideoRef, taker: &str) -> Error {
    Error::new(ErrorCode::Usage, format!("{taker} takes a torrent, not a title such as {video}"))
        .with_hint(format!("run torrential sources {video} to list its torrents, and pass the ID of one"))
        .with_next(format!("torrential sources {video}"))
}

fn invalid_ref(input: &str, why: &str) -> Error {
    let message = if input.contains(':') {
        "a URL is not a torrent reference unless it is a magnet link or an http or https URL".to_string()
    } else {
        format!("{input:?} is not a torrent reference: {why}")
    };
    Error::new(ErrorCode::InvalidRef, message)
        .with_hint("pass an ID from `torrential search`, a magnet link, a .torrent file, or a .torrent URL")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

    #[test]
    fn parses_each_kind_of_target() {
        let cwd = Path::new("/nonexistent");
        assert_eq!(Target::parse("9b5212", cwd).unwrap(), Target::Prefix("9b5212".into()));
        assert_eq!(Target::parse("U9B52", cwd).unwrap(), Target::Prefix("u9b52".into()));
        let full = Target::parse(ARCH, cwd).unwrap();
        assert_eq!(full, Target::InfoHash(InfoHash::V1(decode_hex(ARCH).unwrap())));
        let magnet = format!("magnet:?xt=urn:btih:{ARCH}");
        assert_eq!(Target::parse(&magnet, cwd).unwrap(), Target::Magnet(magnet.clone()));
        assert_eq!(
            Target::parse("https://example.org/a.torrent", cwd).unwrap(),
            Target::TorrentUrl("https://example.org/a.torrent".into())
        );
        assert_eq!(Target::parse("tt0063350", cwd).unwrap(), Target::Title("tt0063350".parse().unwrap()));
        assert_eq!(Target::parse(" tt0903747:3:5 ", cwd).unwrap(), Target::Title("tt0903747:3:5".parse().unwrap()));
        assert_eq!(Target::parse("9b5", cwd).unwrap_err().code, ErrorCode::InvalidRef);
        assert_eq!(Target::parse("tt12", cwd).unwrap_err().code, ErrorCode::InvalidRef);
        assert_eq!(Target::parse("missing.torrent", cwd).unwrap_err().code, ErrorCode::InvalidRef);
    }

    #[test]
    fn a_file_named_like_an_id_does_not_hide_the_id() {
        let cwd = std::env::temp_dir().join(format!("torrential-id-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("9b5212"), b"").unwrap();
        std::fs::write(cwd.join("film"), b"").unwrap();
        let parsed = (Target::parse("9b5212", &cwd), Target::parse("film", &cwd), Target::parse("./9b5212", &cwd));
        std::fs::remove_dir_all(&cwd).unwrap();
        assert_eq!(parsed.0.unwrap(), Target::Prefix("9b5212".into()));
        assert_eq!(parsed.1.unwrap(), Target::TorrentFile(cwd.join("film")));
        assert_eq!(parsed.2.unwrap(), Target::TorrentFile(cwd.join("./9b5212")));
    }

    #[test]
    fn multibyte_input_is_rejected_without_panicking() {
        let err = Target::parse("abcdefgé", Path::new("/nonexistent")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRef);
    }
}
