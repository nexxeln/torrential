mod academic;
mod apibay;
mod archive_org;
mod archlinux;
mod debian;
mod hashes;
mod knaben;
mod listing;
mod nyaa;
mod pages;
mod rss;
mod torznab;
mod ubuntu;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use torrential_core::{Error, ErrorCode, SourceOutcome, text};
use url::Url;

use crate::http::{FetchError, Http};
use crate::observation::Observation;
use crate::store::Store;
use crate::terms::Terms;
use crate::{SearchConfig, TorznabConfig};
use torznab::Torznab;

struct Row {
    name: &'static str,
    /// Lower leads. A release that several sources list takes its name, size and links from the
    /// one with the least, and their reports are listed in this order.
    authority: u16,
    lawful: bool,
    /// It can list films and series, so a title's search asks it.
    video: bool,
    base: &'static str,
    adapter: Adapter,
}

const BUILTIN: [Row; 8] = [
    Row {
        name: "archlinux",
        authority: 0,
        lawful: true,
        video: false,
        base: "https://archlinux.org/",
        adapter: Adapter::Archlinux,
    },
    Row {
        name: "ubuntu",
        authority: 1,
        lawful: true,
        video: false,
        base: "https://releases.ubuntu.com/",
        adapter: Adapter::Ubuntu,
    },
    Row {
        name: "debian",
        authority: 2,
        lawful: true,
        video: false,
        base: "https://cdimage.debian.org/",
        adapter: Adapter::Debian,
    },
    Row {
        name: "archive.org",
        authority: 3,
        lawful: true,
        video: true,
        base: "https://archive.org/",
        adapter: Adapter::ArchiveOrg,
    },
    Row {
        name: "academictorrents",
        authority: 4,
        lawful: true,
        video: false,
        base: "https://academictorrents.com/",
        adapter: Adapter::AcademicTorrents,
    },
    Row {
        name: "knaben",
        authority: 5,
        lawful: false,
        video: true,
        base: "https://api.knaben.org/",
        adapter: Adapter::Knaben,
    },
    Row {
        name: "apibay",
        authority: 6,
        lawful: false,
        video: true,
        base: "https://apibay.org/",
        adapter: Adapter::Apibay,
    },
    Row { name: "nyaa", authority: 7, lawful: false, video: true, base: "https://nyaa.si/", adapter: Adapter::Nyaa },
];

pub(crate) enum Adapter {
    Archlinux,
    Ubuntu,
    Debian,
    ArchiveOrg,
    AcademicTorrents,
    Knaben,
    Apibay,
    Nyaa,
    Torznab(Torznab),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct SourceMeta {
    pub name: String,
    pub authority: u16,
    pub lawful: bool,
    /// A private tracker's announce URLs carry the user's passkey, so only trackers
    /// from public sources are shared across listings of the same torrent.
    pub public: bool,
}

pub(crate) struct Source {
    pub meta: Arc<SourceMeta>,
    /// It can list films and series. A Torznab indexer is assumed to.
    pub video: bool,
    base: Url,
    adapter: Adapter,
}

/// A `.torrent` link from a configured Torznab source, as an engine saves it for the next one:
/// without the API key it was issued under. [`crate::Search::torrent_url`] puts the source's
/// current key back.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SavedLink {
    /// The source's name in the config.
    pub source: String,
    /// The link without the query parameters whose value was the key.
    pub url: String,
    /// Those parameters' names, such as `apikey` or Jackett's `jackett_apikey`.
    pub key_params: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Failure {
    Timeout,
    Http(u16),
    Connect(String),
    BadResponse(String),
    Irrelevant,
}

impl Failure {
    pub fn outcome(&self) -> SourceOutcome {
        let clean = |reason: &str| text::truncate(&text::sanitize(reason), 160);
        match self {
            Self::Timeout => SourceOutcome::Timeout,
            Self::Http(status) => SourceOutcome::Http { status: *status },
            Self::Connect(reason) => SourceOutcome::Connect { reason: clean(reason) },
            Self::BadResponse(reason) => SourceOutcome::BadResponse { reason: clean(reason) },
            Self::Irrelevant => SourceOutcome::Irrelevant,
        }
    }
}

impl From<FetchError> for Failure {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Timeout(_) => Self::Timeout,
            FetchError::Status(status) => Self::Http(status),
            FetchError::Connect(reason) => Self::Connect(reason),
            FetchError::TooLarge(cap) => Self::BadResponse(format!("response larger than {cap} bytes")),
            FetchError::Decode(reason) => Self::BadResponse(reason),
        }
    }
}

pub(crate) struct Stores {
    hashes: Arc<hashes::Hashes>,
    academic: academic::Catalog,
    pages: pages::Pages,
}

impl Stores {
    pub fn new(cache_dir: Option<PathBuf>) -> Stores {
        let store = cache_dir.clone().map(|dir| Arc::new(Store::Disk(dir)));
        Stores {
            hashes: Arc::new(hashes::Hashes::new(cache_dir, store.clone())),
            academic: academic::Catalog::new(store.clone()),
            pages: pages::Pages::new(store),
        }
    }

    pub async fn save(&self) {
        self.hashes.save().await;
    }
}

pub(crate) struct Ctx<'a> {
    pub http: &'a Http,
    pub stores: &'a Stores,
    pub terms: &'a Terms,
    pub adult: bool,
    pub fresh: bool,
    pub deadline: Instant,
    pub partial: AtomicBool,
}

impl Ctx<'_> {
    pub fn mark_partial(&self) {
        self.partial.store(true, Ordering::Relaxed);
    }
}

impl Source {
    /// What the answers cached for this source depend on beside the query.
    pub fn scope(&self) -> String {
        match &self.adapter {
            Adapter::Torznab(torznab) => torznab.scope(&self.base),
            _ => self.base.to_string(),
        }
    }

    /// The link at `url` as a later engine keeps it. Only a Torznab source's links are saved.
    pub fn save_link(&self, url: &str) -> Option<SavedLink> {
        let Adapter::Torznab(torznab) = &self.adapter else { return None };
        torznab.save_link(&self.meta.name, url)
    }

    /// A saved link with the key it needs now.
    pub fn link(&self, saved: &SavedLink) -> Option<String> {
        match &self.adapter {
            Adapter::Torznab(torznab) => torznab.link(saved),
            _ => saved.key_params.is_empty().then(|| Url::parse(&saved.url).ok().map(String::from))?,
        }
    }

    pub fn archive_item(&self, name: &str, web_seeds: &[String]) -> Option<archive_org::Item> {
        match self.adapter {
            Adapter::ArchiveOrg => archive_org::item(&self.base, name, web_seeds),
            _ => None,
        }
    }

    pub async fn search(&self, ctx: &Ctx<'_>) -> Result<Vec<Observation>, Failure> {
        let base = &self.base;
        match &self.adapter {
            Adapter::Archlinux => archlinux::search(ctx, base).await,
            Adapter::Ubuntu => ubuntu::search(ctx, base).await,
            Adapter::Debian => debian::search(ctx, base).await,
            Adapter::ArchiveOrg => archive_org::search(ctx, base).await,
            Adapter::AcademicTorrents => academic::search(ctx, base).await,
            Adapter::Knaben => knaben::search(ctx, base).await,
            Adapter::Apibay => apibay::search(ctx, base).await,
            Adapter::Nyaa => nyaa::search(ctx, base).await,
            Adapter::Torznab(torznab) => torznab.search(ctx, base).await,
        }
    }
}

/// Whether the built-in source named `name` is a lawful catalog. A Torznab indexer never is.
pub fn lawful(name: &str) -> bool {
    BUILTIN.iter().any(|row| row.name == name && row.lawful)
}

pub(crate) fn registry(config: &SearchConfig) -> Result<Vec<Arc<Source>>, Error> {
    for name in config.endpoints.keys() {
        if !BUILTIN.iter().any(|row| row.name == name) {
            return Err(config_error(format!("endpoints names an unknown source {name:?}")));
        }
    }
    let mut sources: Vec<Arc<Source>> = Vec::new();
    for row in BUILTIN {
        let base = base_url(row.name, config.endpoints.get(row.name).map_or(row.base, String::as_str))?;
        let meta = SourceMeta { name: row.name.into(), authority: row.authority, lawful: row.lawful, public: true };
        let source = Source { meta: Arc::new(meta), video: row.video, base, adapter: row.adapter };
        sources.push(Arc::new(source));
    }
    for (after, TorznabConfig { name, url, api_key }) in (BUILTIN.len()..).zip(&config.torznab) {
        let valid = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
        if !valid {
            return Err(config_error(format!(
                "torznab source name {name:?} must be lowercase letters, digits, '-', '_', or '.'"
            )));
        }
        if sources.iter().any(|s| s.meta.name == *name) {
            return Err(config_error(format!("source name {name:?} is used twice")));
        }
        let authority = u16::try_from(after).unwrap_or(u16::MAX);
        let meta = SourceMeta { name: name.clone(), authority, lawful: false, public: false };
        let base = url::Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| config_error(format!("torznab source {name:?} needs an http or https URL")))?;
        let adapter = Adapter::Torznab(Torznab { api_key: api_key.clone() });
        sources.push(Arc::new(Source { meta: Arc::new(meta), video: true, base, adapter }));
    }
    for name in &config.disabled {
        if !sources.iter().any(|s| s.meta.name == *name) {
            return Err(config_error(format!("sources.disabled lists an unknown source {name:?}")));
        }
    }
    sources.retain(|s| !config.disabled.contains(&s.meta.name));
    Ok(sources)
}

fn base_url(name: &str, raw: &str) -> Result<Url, Error> {
    let mut url = Url::parse(raw)
        .ok()
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .ok_or_else(|| config_error(format!("the endpoint for {name} must be an http or https URL")))?;
    // Relative joins replace the last path segment unless the base ends in a slash.
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

fn config_error(message: String) -> Error {
    Error::new(ErrorCode::Usage, message).with_hint("fix the sources section of the torrential config")
}

fn at(base: &Url, path: &str) -> Url {
    base.join(path).expect("an http base accepts a relative path")
}

#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;

    pub fn read(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jackett(key: Option<&str>) -> Arc<Source> {
        let torznab = TorznabConfig {
            name: "jackett".into(),
            url: "http://127.0.0.1:9117/api".into(),
            api_key: key.map(String::from),
        };
        let config = SearchConfig { torznab: vec![torznab], ..SearchConfig::default() };
        registry(&config).unwrap().into_iter().find(|s| s.meta.name == "jackett").unwrap()
    }

    #[test]
    fn a_saved_link_leaves_out_the_key_and_takes_the_current_one_back() {
        let save = |key, url: &str| jackett(key).save_link(url).map(|l| (l.url, l.key_params));
        let issued = "http://127.0.0.1:9117/dl/x/?jackett_apikey=k3y&path=a%2Bb%3D&file=Ubuntu+26.04";
        let saved = jackett(Some("k3y")).save_link(issued).unwrap();

        assert_eq!(
            saved,
            SavedLink {
                source: "jackett".into(),
                url: "http://127.0.0.1:9117/dl/x/?path=a%2Bb%3D&file=Ubuntu+26.04".into(),
                key_params: vec!["jackett_apikey".into()],
            }
        );
        assert_eq!(
            jackett(Some("n3w")).link(&saved).as_deref(),
            Some("http://127.0.0.1:9117/dl/x/?path=a%2Bb%3D&file=Ubuntu+26.04&jackett_apikey=n3w")
        );
        assert_eq!(jackett(None).link(&saved), None);
        assert_eq!(
            save(Some("k3y"), "http://h/1/download?apikey=k3y&link=x"),
            Some(("http://h/1/download?link=x".into(), vec!["apikey".into()]))
        );
        assert_eq!(save(Some("k3y"), "http://h/dl/k3y/a.torrent?apikey=k3y"), None, "a key in the path");
        assert_eq!(save(Some("k3y"), "http://h/dl?apikey=k3y&next=%2Fget%3Fkey%3Dk3y"), None, "an encoded key");
        assert_eq!(save(None, "http://h/dl?id=7"), Some(("http://h/dl?id=7".into(), Vec::new())));
    }
}
