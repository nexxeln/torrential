use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, ErrorCode};

/// The identity torrential uses everywhere: the v1 infohash, or the v2 infohash
/// truncated to 20 bytes, which is what BitTorrent v2 uses on the DHT and trackers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TorrentId([u8; 20]);

impl TorrentId {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    pub fn has_prefix(&self, prefix: &HexPrefix) -> bool {
        self.to_hex().starts_with(prefix.as_str())
    }
}

impl fmt::Display for TorrentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for TorrentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TorrentId({})", self.to_hex())
    }
}

impl FromStr for TorrentId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        decode_hex::<20>(s).map(Self).ok_or_else(|| invalid_ref(s, "a torrent ID is 40 hex characters"))
    }
}

impl Serialize for TorrentId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for TorrentId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A torrent's infohash. A hybrid torrent carries both, and both name the same torrent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum InfoHash {
    V1([u8; 20]),
    V2([u8; 32]),
    Hybrid { v1: [u8; 20], v2: [u8; 32] },
}

impl InfoHash {
    pub fn id(&self) -> TorrentId {
        match self {
            Self::V1(v1) | Self::Hybrid { v1, .. } => TorrentId(*v1),
            Self::V2(v2) => {
                let mut truncated = [0; 20];
                truncated.copy_from_slice(&v2[..20]);
                TorrentId(truncated)
            }
        }
    }

    pub fn v1(&self) -> Option<[u8; 20]> {
        match self {
            Self::V1(v1) | Self::Hybrid { v1, .. } => Some(*v1),
            Self::V2(_) => None,
        }
    }

    pub fn v2(&self) -> Option<[u8; 32]> {
        match self {
            Self::V2(v2) | Self::Hybrid { v2, .. } => Some(*v2),
            Self::V1(_) => None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct InfoHashWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v2: Option<String>,
}

impl Serialize for InfoHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        InfoHashWire { v1: self.v1().map(|h| hex(&h)), v2: self.v2().map(|h| hex(&h)) }.serialize(s)
    }
}

impl<'de> Deserialize<'de> for InfoHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = InfoHashWire::deserialize(d)?;
        let v1 = wire.v1.as_deref().map(decode_hex::<20>);
        let v2 = wire.v2.as_deref().map(decode_hex::<32>);
        match (v1, v2) {
            (Some(Some(v1)), None) => Ok(Self::V1(v1)),
            (None, Some(Some(v2))) => Ok(Self::V2(v2)),
            (Some(Some(v1)), Some(Some(v2))) => Ok(Self::Hybrid { v1, v2 }),
            _ => Err(serde::de::Error::custom("infohash needs a valid v1 or v2 hex digest")),
        }
    }
}

/// Identifies a search result whose infohash is not known yet: an indexer row that
/// only links a `.torrent` file. Shown with a `u` prefix. The bytes are a digest of
/// the `.torrent` URL, so the same row gets the same ID in every search.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LocatorId([u8; 20]);

impl LocatorId {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }
}

/// The ID of a search result: a torrent, or a locator until its `.torrent` is fetched.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReleaseId {
    Torrent(TorrentId),
    Locator(LocatorId),
}

impl ReleaseId {
    /// The full ID as typed by users: 40 hex characters, or `u` and 40 hex characters.
    pub fn to_ref(&self) -> String {
        match self {
            Self::Torrent(id) => id.to_hex(),
            Self::Locator(id) => format!("u{}", id.to_hex()),
        }
    }
}

impl Serialize for ReleaseId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_ref())
    }
}

impl<'de> Deserialize<'de> for ReleaseId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let bad = || serde::de::Error::custom(format!("invalid release id {s:?}"));
        match s.strip_prefix('u') {
            Some(rest) => decode_hex::<20>(rest).map(|b| Self::Locator(LocatorId(b))).ok_or_else(bad),
            None => decode_hex::<20>(&s).map(|b| Self::Torrent(TorrentId(b))).ok_or_else(bad),
        }
    }
}

/// A unique-prefix reference: 4 to 40 lowercase hex characters, optionally led by `u`
/// for a locator ID.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct HexPrefix {
    locator: bool,
    hex: String,
}

impl HexPrefix {
    pub const MIN_LEN: usize = 4;

    pub fn as_str(&self) -> &str {
        &self.hex
    }

    pub fn is_locator(&self) -> bool {
        self.locator
    }

    pub fn matches(&self, id: &ReleaseId) -> bool {
        match id {
            ReleaseId::Torrent(t) => !self.locator && t.to_hex().starts_with(&self.hex),
            ReleaseId::Locator(l) => self.locator && l.to_hex().starts_with(&self.hex),
        }
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
            Err(invalid_ref(s, "an ID is 4 to 40 hex characters, or u and hex for a result without an infohash"))
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
}

impl Target {
    /// Parses user input in a fixed order: magnet link, URL, full infohash, `.torrent`
    /// path, ID prefix, then any other existing file. `cwd` resolves relative paths.
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
        match input.parse::<HexPrefix>() {
            Ok(prefix) => Ok(Self::Prefix(if prefix.locator { format!("u{}", prefix.hex) } else { prefix.hex })),
            Err(_) if path.is_file() => Ok(Self::TorrentFile(path)),
            Err(err) => Err(err),
        }
    }
}

/// A 40-character hex or 32-character base32 v1 infohash, or a 64-character hex v2 infohash.
pub fn parse_full_infohash(s: &str) -> Option<InfoHash> {
    match s.len() {
        40 => decode_hex::<20>(s).map(InfoHash::V1),
        32 => decode_base32_20(s).map(InfoHash::V1),
        64 => decode_hex::<32>(s).map(InfoHash::V2),
        _ => None,
    }
}

/// The shortest prefix of `id`, at least `min` characters, that no ID in `others` shares.
pub fn shortest_unique_prefix<'a>(
    id: &TorrentId,
    others: impl IntoIterator<Item = &'a TorrentId>,
    min: usize,
) -> String {
    let hex = id.to_hex();
    let longest_shared = others
        .into_iter()
        .filter(|other| *other != id)
        .map(|other| common_prefix_len(&hex, &other.to_hex()))
        .max()
        .unwrap_or(0);
    hex[..(longest_shared + 1).max(min).min(40)].to_string()
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0xf) as usize] as char);
    }
    out
}

pub(crate) fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0; N];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

fn decode_base32_20(s: &str) -> Option<[u8; 20]> {
    let mut bits: u64 = 0;
    let mut nbits = 0;
    let mut out = Vec::with_capacity(20);
    for c in s.bytes() {
        let v = match c.to_ascii_uppercase() {
            b'A'..=b'Z' => c.to_ascii_uppercase() - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(v);
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
            bits &= (1 << nbits) - 1;
        }
    }
    out.try_into().ok()
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
    use super::*;
    use std::path::Path;

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
        assert_eq!(Target::parse("9b5", cwd).unwrap_err().code, ErrorCode::InvalidRef);
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

    #[test]
    fn a_minimum_longer_than_an_id_returns_the_whole_id() {
        let id: TorrentId = ARCH.parse().unwrap();
        assert_eq!(shortest_unique_prefix(&id, [&id], 64), ARCH);
    }

    #[test]
    fn base32_and_hex_infohashes_agree() {
        let from_hex = parse_full_infohash(ARCH).unwrap();
        let from_b32 = parse_full_infohash("TNJBEGN4JBKQCQFGM33UUKIER3LY5EZ3").unwrap();
        assert_eq!(from_hex, from_b32);
    }

    #[test]
    fn v2_ids_truncate_to_twenty_bytes() {
        let v2 = [0xab; 32];
        assert_eq!(InfoHash::V2(v2).id().to_hex(), "ab".repeat(20));
        let hybrid = InfoHash::Hybrid { v1: [0x01; 20], v2 };
        assert_eq!(hybrid.id().to_hex(), "01".repeat(20));
    }

    #[test]
    fn shortest_unique_prefix_grows_past_collisions() {
        let a: TorrentId = format!("9b5212{}", "0".repeat(34)).parse().unwrap();
        let b: TorrentId = format!("9b5212f{}", "0".repeat(33)).parse().unwrap();
        let c: TorrentId = format!("51e0c7{}", "0".repeat(34)).parse().unwrap();
        assert_eq!(shortest_unique_prefix(&a, [&a, &b, &c], 6), "9b52120");
        assert_eq!(shortest_unique_prefix(&c, [&a, &b, &c], 6), "51e0c7");
    }

    #[test]
    fn release_ids_round_trip_through_json() {
        let locator = ReleaseId::Locator(LocatorId::from_bytes([0x0f; 20]));
        let json = serde_json::to_string(&locator).unwrap();
        assert_eq!(json, format!("\"u{}\"", "0f".repeat(20)));
        assert_eq!(serde_json::from_str::<ReleaseId>(&json).unwrap(), locator);
    }
}
