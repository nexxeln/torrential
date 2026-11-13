use std::time::Duration;

use serde::Deserialize;
use tokio::time::Instant;
use torrential_core::{Category, Error, ErrorCode, Flag};
use url::Url;

use super::{Ctx, Failure, at};
use crate::http::Http;
use crate::observation::{Identity, Observation, infohash, web_url};

const FIELDS: [&str; 10] = [
    "identifier",
    "title",
    "btih",
    "item_size",
    "publicdate",
    "downloads",
    "licenseurl",
    "mediatype",
    "color",
    "format",
];

/// Archive.org's names for video files, most common first.
const VIDEO_FORMATS: [&str; 14] = [
    "Ogg Video",
    "h.264",
    "Cinepack",
    "MPEG4",
    "512Kb MPEG4",
    "h.264 IA",
    "MPEG2",
    "Motion JPEG",
    "DivX",
    "HiRes MPEG4",
    "Matroska",
    "WebM",
    "QuickTime",
    "Ogg Theora",
];

pub(super) async fn search(ctx: &Ctx<'_>, base: &Url) -> Result<Vec<Observation>, Failure> {
    let mut url = at(base, "advancedsearch.php");
    {
        let q = format!("title:({}) AND btih:* AND NOT mediatype:collection", ctx.terms.words().join(" "));
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", &q);
        for field in FIELDS {
            pairs.append_pair("fl[]", field);
        }
        pairs.append_pair("sort[]", "downloads desc").append_pair("rows", "50").append_pair("output", "json");
    }
    let body = ctx.http.get(&url, 4 << 20, ctx.deadline).await?;
    parse(&body, base).map_err(Failure::BadResponse)
}

#[derive(Deserialize)]
struct Feed {
    response: Response,
}

#[derive(Deserialize)]
struct Response {
    docs: Vec<Doc>,
}

#[derive(Deserialize)]
struct Doc {
    identifier: String,
    title: Option<Text>,
    btih: Option<Text>,
    item_size: Option<u64>,
    downloads: Option<u64>,
    publicdate: Option<Text>,
    licenseurl: Option<Text>,
    mediatype: Option<Text>,
    color: Option<Text>,
    format: Option<Text>,
}

/// Archive.org returns a list where an item has several values for a field.
#[derive(Deserialize)]
#[serde(untagged)]
enum Text {
    One(String),
    Many(Vec<String>),
}

impl Text {
    fn all(&self) -> &[String] {
        match self {
            Text::One(s) => std::slice::from_ref(s),
            Text::Many(v) => v,
        }
    }

    fn first(&self) -> Option<&str> {
        self.all().first().map(String::as_str)
    }
}

fn parse(body: &[u8], base: &Url) -> Result<Vec<Observation>, String> {
    let feed: Feed = serde_json::from_slice(body).map_err(|e| format!("advancedsearch JSON: {e}"))?;
    Ok(feed.response.docs.into_iter().filter_map(|doc| observation(doc, base)).collect())
}

/// Archive.org identifiers start with a letter or digit, then letters, digits, `-`, `_`, and `.`.
fn is_identifier(id: &str) -> bool {
    id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn observation(doc: Doc, base: &Url) -> Option<Observation> {
    let id = &doc.identifier;
    if !is_identifier(id) {
        return None;
    }
    let hash = doc.btih.as_ref().and_then(Text::first).and_then(infohash)?;
    let torrent_url = web_url(at(base, &format!("download/{id}/{id}_archive.torrent")).as_str());
    let name = doc.title.as_ref().and_then(Text::first).unwrap_or(id);
    let license = doc.licenseurl.as_ref().and_then(Text::first).unwrap_or_default().to_ascii_lowercase();
    let mut obs = Observation::new(Identity::Hash { hash, torrent_url }, name)?
        .flag(Flag::PublicDomain, license.contains("publicdomain"))
        .flag(
            Flag::CreativeCommons,
            !license.contains("publicdomain") && license.contains("creativecommons.org/licenses"),
        );
    obs.size = doc.item_size;
    obs.downloads = doc.downloads;
    obs.published = doc.publicdate.as_ref().and_then(Text::first).and_then(crate::dates::rfc3339);
    obs.category = category(&doc);
    obs.info_url = web_url(at(base, &format!("details/{id}")).as_str());
    Some(obs)
}

fn category(doc: &Doc) -> Option<Category> {
    let video = || doc.format.as_ref().is_some_and(|f| f.all().iter().any(|f| VIDEO_FORMATS.contains(&f.as_str())));
    Some(match doc.mediatype.as_ref().and_then(Text::first)? {
        "movies" => Category::Video,
        // Archive.org files Big Buck Bunny under audio, with color and video formats.
        "audio" if doc.color.is_some() && video() => Category::Video,
        "audio" | "etree" => Category::Audio,
        "software" => Category::Software,
        "texts" => Category::Book,
        "data" => Category::Dataset,
        _ => Category::Other,
    })
}

#[derive(PartialEq, Eq, Debug)]
pub(crate) struct Item {
    identifier: String,
    metadata: Url,
    scheme: String,
}

/// libtorrent asks a multi-file torrent's web seed for `<seed><torrent name>/<path>`, so a torrent
/// that works from the download URL is named after its item. Older torrents list it over http.
pub(super) fn item(base: &Url, name: &str, web_seeds: &[String]) -> Option<Item> {
    let download = at(base, "download/");
    let listed = web_seeds.iter().filter_map(|seed| Url::parse(seed).ok()).any(|seed| {
        (seed.host_str(), seed.port(), seed.path()) == (download.host_str(), download.port(), download.path())
    });
    (listed && is_identifier(name)).then(|| Item {
        identifier: name.to_string(),
        metadata: at(base, &format!("metadata/{name}")),
        scheme: base.scheme().to_string(),
    })
}

impl Item {
    pub async fn mirrors(&self, http: &Http) -> Result<Vec<String>, Error> {
        const CAP: usize = 16 << 20;
        const WITHIN: Duration = Duration::from_secs(20);
        let body = http.get(&self.metadata, CAP, Instant::now() + WITHIN).await;
        let why = match body {
            Ok(body) => match mirrors(&body, self) {
                Ok(seeds) => return Ok(seeds),
                Err(why) => why,
            },
            Err(e) => crate::fetch_reason(e),
        };
        Err(Error::new(
            ErrorCode::FetchFailed,
            format!("archive.org did not name the servers of {}: {why}", self.identifier),
        ))
    }
}

#[derive(Deserialize)]
struct Metadata {
    d1: Option<String>,
    d2: Option<String>,
    /// Where the item lives on each server, such as `/0/items/<identifier>`. Absent for an
    /// unknown item, whose metadata is `{}`.
    dir: Option<String>,
}

/// The servers' item directories serve each file without a redirect. libtorrent appends the
/// torrent's name, the identifier, so each seed is the folder that holds the item's directory.
fn mirrors(body: &[u8], item: &Item) -> Result<Vec<String>, String> {
    let meta: Metadata = serde_json::from_slice(body).map_err(|e| format!("metadata JSON: {e}"))?;
    let dir = meta.dir.ok_or("its metadata has no directory")?;
    let parent = dir
        .strip_suffix(&format!("/{}", item.identifier))
        .filter(|p| p.starts_with('/') && p[1..].split('/').all(is_identifier))
        .ok_or_else(|| format!("its directory {dir:?} does not hold it by name"))?;
    let host = |h: &String| !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.:".contains(&b));
    let seeds: Vec<String> = [meta.d1, meta.d2]
        .into_iter()
        .flatten()
        .filter(host)
        .map(|h| format!("{}://{h}{parent}/", item.scheme))
        .collect();
    if seeds.is_empty() {
        return Err("its metadata names no server".into());
    }
    Ok(seeds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    #[test]
    fn reads_items_with_their_licenses() {
        let base = Url::parse("https://archive.org/").unwrap();
        let all = parse(&fixture::read("archive.org/advancedsearch.php-night-of-the-living-dead.json"), &base).unwrap();
        assert_eq!(all.len(), 50);
        let dvd = all
            .iter()
            .find(|o| o.info_url.as_deref() == Some("https://archive.org/details/night_of_the_living_dead_dvd"))
            .unwrap();
        assert_eq!(dvd.name(), "Night of the Living Dead");
        assert_eq!(
            dvd.identity,
            Identity::Hash {
                hash: infohash("6bc1af23a17fd62156ab2f0e41d3c86769c2c66c").unwrap(),
                torrent_url: Some(
                    "https://archive.org/download/night_of_the_living_dead_dvd/night_of_the_living_dead_dvd_archive.torrent".into()
                ),
            }
        );
        assert_eq!(dvd.size, Some(5_003_733_941));
        assert_eq!((dvd.peers, dvd.downloads), (None, Some(305_139)));
        assert_eq!(dvd.category, Some(Category::Video));
        assert_eq!(dvd.flags.iter().collect::<Vec<_>>(), [&Flag::PublicDomain]);
        assert_eq!(dvd.published, Some(1_177_371_285));
    }

    #[test]
    fn a_film_filed_as_audio_is_video() {
        let base = Url::parse("https://archive.org/").unwrap();
        let all = parse(&fixture::read("archive.org/advancedsearch.php-big-buck-bunny.json"), &base).unwrap();
        let category = |id: &str| {
            let page = format!("https://archive.org/details/{id}");
            all.iter().find(|o| o.info_url.as_deref() == Some(page.as_str())).unwrap().category
        };
        let ids = ["BigBuckBunny", "bigbuckbunnyav1", "JanMorgenstern-BigBuckBunny", "BigBuckBunny_124"];
        assert_eq!(
            ids.map(category),
            [Some(Category::Video), Some(Category::Video), Some(Category::Audio), Some(Category::Video)]
        );
    }

    fn seeds(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_torrent_that_lists_the_download_url_downloads_from_the_item_it_is_named_after() {
        let base = Url::parse("https://archive.org/").unwrap();
        let caminandes =
            seeds(&["https://archive.org/download/", "http://ia600208.us.archive.org/6/items/", "/6/items/"]);
        let item = item(&base, "Caminandes1LlamaDrama", &caminandes).unwrap();
        assert_eq!(item.metadata.as_str(), "https://archive.org/metadata/Caminandes1LlamaDrama");
        let old = seeds(&["http://archive.org/download/", "http://ia600308.us.archive.org/35/items"]);
        assert_eq!(
            super::item(&base, "night_of_the_living_dead_dvd", &old).unwrap().identifier,
            "night_of_the_living_dead_dvd"
        );
        for list in [&["http://dn801201.us.archive.org/0/items/"][..], &["https://archive.org/details/"], &[]] {
            assert_eq!(super::item(&base, "BigBuckBunny_124", &seeds(list)), None, "{list:?}");
        }
        assert_eq!(super::item(&base, "..", &caminandes), None);
    }

    #[test]
    fn an_items_servers_serve_the_folder_that_holds_its_directory() {
        let base = Url::parse("https://archive.org/").unwrap();
        let item = item(&base, "Caminandes1LlamaDrama", &seeds(&["https://archive.org/download/"])).unwrap();
        let recorded = fixture::read("archive.org/metadata-Caminandes1LlamaDrama.json");
        assert_eq!(mirrors(&recorded, &item).unwrap(), ["https://dn600203.us.archive.org/0/items/"]);
        let both = br#"{"d1": "ia800208.us.archive.org", "d2": "ia600208.us.archive.org", "dir": "/6/items/Caminandes1LlamaDrama"}"#;
        assert_eq!(
            mirrors(both, &item).unwrap(),
            ["https://ia800208.us.archive.org/6/items/", "https://ia600208.us.archive.org/6/items/"]
        );
        let refused = [
            (&br#"{}"#[..], "its metadata has no directory"),
            (
                br#"{"d1": "a.archive.org", "dir": "/0/items/Caminandes2"}"#,
                r#"its directory "/0/items/Caminandes2" does not hold it by name"#,
            ),
            (
                br#"{"d1": "a.archive.org", "dir": "/../Caminandes1LlamaDrama"}"#,
                r#"its directory "/../Caminandes1LlamaDrama" does not hold it by name"#,
            ),
            (br#"{"d1": "evil.example/x?", "dir": "/0/items/Caminandes1LlamaDrama"}"#, "its metadata names no server"),
        ];
        for (body, why) in refused {
            assert_eq!(mirrors(body, &item).unwrap_err(), why);
        }
    }
}
