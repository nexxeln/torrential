use torrential_core::id::parse_full_infohash;
use torrential_core::text::sanitize;
use torrential_core::{Error, ErrorCode, InfoHash};

#[derive(Clone, PartialEq, Debug)]
pub struct Magnet {
    pub info_hash: InfoHash,
    pub name: Option<String>,
    pub exact_length: Option<u64>,
    pub uri: String,
}

/// The multihash prefix of a SHA-256 digest: code 0x12, length 0x20.
const BTMH_SHA256: &str = "1220";

pub fn parse(uri: &str) -> Result<Magnet, Error> {
    let invalid = |why: &str| {
        Error::new(ErrorCode::InvalidRef, format!("not a valid magnet link: {why}"))
            .with_hint("pass the whole magnet link in single quotes")
    };
    let url = url::Url::parse(uri).map_err(|_| invalid("it does not parse as a URI"))?;
    if url.scheme() != "magnet" {
        return Err(invalid("it does not start with magnet:?"));
    }
    let (mut v1, mut v2, mut name, mut exact_length) = (None, None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "xt" => {
                if let Some(hash) = value.strip_prefix("urn:btih:") {
                    v1 = match parse_full_infohash(hash) {
                        Some(InfoHash::V1(h)) => Some(h),
                        _ => return Err(invalid("its btih hash is not 40 hex or 32 base32 characters")),
                    };
                } else if let Some(hash) = value.strip_prefix("urn:btmh:") {
                    let digest =
                        hash.strip_prefix(BTMH_SHA256).ok_or_else(|| invalid("its btmh hash is not SHA-256"))?;
                    v2 = match parse_full_infohash(digest) {
                        Some(InfoHash::V2(h)) => Some(h),
                        _ => return Err(invalid("its btmh hash is not 64 hex characters")),
                    };
                }
            }
            "dn" => name = Some(sanitize(&value)).filter(|n| !n.is_empty()),
            "xl" => exact_length = value.parse().ok(),
            _ => {}
        }
    }
    let info_hash = match (v1, v2) {
        (Some(v1), Some(v2)) => InfoHash::Hybrid { v1, v2 },
        (Some(v1), None) => InfoHash::V1(v1),
        (None, Some(v2)) => InfoHash::V2(v2),
        (None, None) => return Err(invalid("it has no btih or btmh hash")),
    };
    Ok(Magnet { info_hash, name, exact_length, uri: uri.to_string() })
}

impl Magnet {
    pub fn has_trackers(&self) -> bool {
        url::Url::parse(&self.uri).is_ok_and(|url| url.query_pairs().any(|(key, _)| key == "tr"))
    }
}

pub fn from_hash(info_hash: InfoHash) -> Magnet {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut uri = "magnet:?".to_string();
    let mut parts = Vec::new();
    if let Some(v1) = info_hash.v1() {
        parts.push(format!("xt=urn:btih:{}", hex(&v1)));
    }
    if let Some(v2) = info_hash.v2() {
        parts.push(format!("xt=urn:btmh:{BTMH_SHA256}{}", hex(&v2)));
    }
    uri.push_str(&parts.join("&"));
    Magnet { info_hash, name: None, exact_length: None, uri }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCH: &str = "9b521219bc48550140a666f74a29048ed78e933b";

    #[test]
    fn reads_hash_name_and_length() {
        let m = parse(&format!(
            "magnet:?xt=urn:btih:{ARCH}&dn=archlinux-2026.10.01-x86_64.iso&xl=1640497152&tr=udp%3A%2F%2Ft.example%3A80"
        ))
        .unwrap();
        assert_eq!(m.info_hash.id().to_hex(), ARCH);
        assert_eq!(m.name.as_deref(), Some("archlinux-2026.10.01-x86_64.iso"));
        assert_eq!(m.exact_length, Some(1_640_497_152));
    }

    #[test]
    fn hybrid_links_carry_both_hashes() {
        let v2 = "ab".repeat(32);
        let m = parse(&format!("magnet:?xt=urn:btih:{ARCH}&xt=urn:btmh:1220{v2}")).unwrap();
        assert_eq!(m.info_hash.v2().map(|h| h[0]), Some(0xab));
        assert_eq!(m.info_hash.id().to_hex(), ARCH);
        assert_eq!(parse(&from_hash(m.info_hash).uri).unwrap().info_hash, m.info_hash);
    }

    #[test]
    fn names_are_sanitized_and_bad_links_refused() {
        let m = parse(&format!("magnet:?xt=urn:btih:{ARCH}&dn=evil%1B%5B31mname")).unwrap();
        assert_eq!(m.name.as_deref(), Some("evilname"));
        assert_eq!(
            parse("magnet:?dn=nothing").unwrap_err().message,
            "not a valid magnet link: it has no btih or btmh hash"
        );
        assert_eq!(parse("magnet:?xt=urn:btih:1234").unwrap_err().code, ErrorCode::InvalidRef);
    }
}
