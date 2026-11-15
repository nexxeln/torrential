use std::collections::BTreeSet;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use torrential_core::id::parse_full_infohash;
use torrential_core::{Category, Codec, Flag, InfoHash};

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Observation {
    pub identity: Identity,
    name: String,
    pub size: Option<u64>,
    pub peers: Option<Peers>,
    pub downloads: Option<u64>,
    pub published: Option<i64>,
    pub category: Option<Category>,
    pub flags: BTreeSet<Flag>,
    pub trackers: Vec<String>,
    pub info_url: Option<String>,
    /// The video files the item holds, as its catalog lists them. Only archive.org lists them, and
    /// only in a title's search.
    pub videos: Vec<VideoFile>,
}

/// A video file of an item, with the size of its picture.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct VideoFile {
    /// Its path in the item, which ends in its extension.
    pub name: String,
    pub size: u64,
    pub width: u32,
    pub height: u32,
    /// `None` when its catalog's format does not say.
    pub codec: Option<VideoCodec>,
}

/// A video's codec, as a catalog's format or a release's name gives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum VideoCodec {
    /// H.264, HEVC or AV1, which a screen may decode in hardware.
    Modern(Codec),
    /// An older codec no browser decodes, such as Theora, XviD, DivX or MPEG-2.
    Old,
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Identity {
    Hash { hash: InfoHash, torrent_url: Option<String> },
    Locator { torrent_url: String },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Peers {
    pub seeders: u32,
    pub leechers: Option<u32>,
}

impl Observation {
    pub fn new(identity: Identity, raw_name: &str) -> Option<Observation> {
        let name = torrential_core::text::sanitize(raw_name);
        (!name.is_empty()).then(|| Observation {
            identity,
            name,
            size: None,
            peers: None,
            downloads: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            trackers: Vec::new(),
            info_url: None,
            videos: Vec::new(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn torrent_url(&self) -> Option<&str> {
        match &self.identity {
            Identity::Hash { torrent_url, .. } => torrent_url.as_deref(),
            Identity::Locator { torrent_url } => Some(torrent_url),
        }
    }

    pub fn flag(mut self, flag: Flag, on: bool) -> Self {
        if on {
            self.flags.insert(flag);
        }
        self
    }
}

impl Peers {
    pub fn new(seeders: Option<u32>, leechers: Option<u32>) -> Option<Peers> {
        seeders.map(|seeders| Peers { seeders, leechers })
    }
}

pub(crate) fn infohash(s: &str) -> Option<InfoHash> {
    let hash = parse_full_infohash(s.trim())?;
    let zero = hash.v1().is_some_and(|h| h == [0; 20]) || hash.v2().is_some_and(|h| h == [0; 32]);
    (!zero).then_some(hash)
}

pub(crate) fn count(s: &str) -> Option<u32> {
    s.trim().parse().ok()
}

pub(crate) fn web_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host().is_some()).then(|| url.to_string())
}

pub(crate) fn tracker(raw: &str) -> Option<String> {
    const MAX_LEN: usize = 512;
    let raw = raw.trim();
    if raw.len() > MAX_LEN {
        return None;
    }
    let mut url = url::Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "udp" | "wss") {
        return None;
    }
    // `url` lowercases hosts only for special schemes, and `udp` is not one.
    let host = url.host_str()?.to_ascii_lowercase();
    url.set_host(Some(&host)).ok()?;
    Some(url.to_string())
}

pub(crate) struct Magnet {
    pub hash: InfoHash,
    pub trackers: Vec<String>,
}

pub(crate) fn parse_magnet(raw: &str) -> Option<Magnet> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if url.scheme() != "magnet" {
        return None;
    }
    let (mut v1, mut v2) = (None, None);
    let mut trackers = Vec::new();
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "xt" => {
                if let Some(h) = value.strip_prefix("urn:btih:") {
                    v1 = infohash(h).and_then(|h| h.v1()).or(v1);
                } else if let Some(h) = value.strip_prefix("urn:btmh:1220") {
                    v2 = infohash(h).and_then(|h| h.v2()).or(v2);
                }
            }
            "tr" => trackers.extend(tracker(&value)),
            _ => {}
        }
    }
    let hash = match (v1, v2) {
        (Some(v1), Some(v2)) => InfoHash::Hybrid { v1, v2 },
        (Some(v1), None) => InfoHash::V1(v1),
        (None, Some(v2)) => InfoHash::V2(v2),
        (None, None) => return None,
    };
    Some(Magnet { hash, trackers })
}

/// Undoes the HTML escaping The Pirate Bay applies to names, which apibay and
/// Knaben pass on.
pub(crate) fn unescape_html(name: &str) -> String {
    name.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Encodes a URL query value with `%20` for spaces. apibay reads `+` literally.
pub(crate) fn percent(s: &str) -> String {
    utf8_percent_encode(s, COMPONENT).to_string()
}

pub(crate) fn build_magnet(hash: &InfoHash, name: &str, trackers: &[String]) -> String {
    let mut out = String::from("magnet:?");
    let mut xts = Vec::new();
    if let Some(v1) = hash.v1() {
        xts.push(format!("urn:btih:{}", hex(&v1)));
    }
    if let Some(v2) = hash.v2() {
        xts.push(format!("urn:btmh:1220{}", hex(&v2)));
    }
    let params = xts
        .iter()
        .map(|xt| format!("xt={xt}"))
        .chain(std::iter::once(format!("dn={}", percent(name))))
        .chain(trackers.iter().map(|tr| format!("tr={}", percent(tr))));
    out.push_str(&params.collect::<Vec<_>>().join("&"));
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

    #[test]
    fn rejects_placeholder_and_malformed_hashes() {
        assert_eq!(infohash("0000000000000000000000000000000000000000"), None);
        assert_eq!(infohash("9b52"), None);
        assert_eq!(infohash(&ARCH.to_uppercase()).map(|h| h.id().to_hex()), Some(ARCH.to_string()));
    }

    #[test]
    fn unknown_counts_stay_unknown() {
        assert_eq!(count(" 37 "), Some(37));
        assert_eq!(count("0"), Some(0));
        assert_eq!(count("-1"), None);
        assert_eq!(count(""), None);
    }

    #[test]
    fn magnets_round_trip_hybrid_hashes_and_trackers() {
        let v2 = "ab".repeat(32);
        let raw = format!(
            "magnet:?xt=urn:btih:{}&xt=urn:btmh:1220{v2}&dn=x&tr=UDP%3A%2F%2FTracker.Example.org%3A1337%2FAnnounce&tr=ftp%3A%2F%2Fno",
            ARCH.to_uppercase()
        );
        let magnet = parse_magnet(&raw).unwrap();
        assert!(matches!(magnet.hash, InfoHash::Hybrid { .. }));
        assert_eq!(magnet.trackers, vec!["udp://tracker.example.org:1337/Announce".to_string()]);
        let built = build_magnet(&magnet.hash, "Night of the Living Dead (1968)", &magnet.trackers);
        assert_eq!(
            built,
            format!(
                "magnet:?xt=urn:btih:{ARCH}&xt=urn:btmh:1220{v2}&dn=Night%20of%20the%20Living%20Dead%20%281968%29&tr=udp%3A%2F%2Ftracker.example.org%3A1337%2FAnnounce"
            )
        );
    }
}
