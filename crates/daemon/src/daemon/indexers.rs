//! The Torznab indexers in config.yaml. The engine adds and removes them, so a command line on
//! another machine changes the engine it drives, and its searches use the new list at once.

use std::sync::Arc;

use tokio::sync::Mutex;
use torrential_core::indexer::{ApiKey, Check, Indexer, IndexerList};
use torrential_core::{Error, ErrorCode};
use torrential_search::indexers::{Found, Probe};
use torrential_search::{SearchConfig, TorznabConfig};
use url::Url;

use super::ctx::Ctx;
use crate::config::{self, Config, IndexerEntry, Indexers as Listed, Key};
use crate::env::Env;
use crate::fsutil::Writes;
use crate::log::log;
use crate::paths::Paths;

/// config.yaml, as the engine reads and writes its indexers.
pub struct Indexers {
    paths: Paths,
    env: Env,
    probe: Arc<Probe>,
    /// One change to config.yaml at a time.
    writing: Mutex<()>,
}

impl Indexers {
    pub fn new(paths: Paths, env: Env) -> Result<Self, Error> {
        Ok(Self { paths, env, probe: Arc::new(Probe::new()?), writing: Mutex::new(()) })
    }

    /// `entries`, each asked at once whether it answers.
    async fn checked(&self, entries: Vec<IndexerEntry>) -> Vec<Indexer> {
        let mut checks = tokio::task::JoinSet::new();
        for (at, entry) in entries.iter().enumerate() {
            let (probe, asked) = (self.probe.clone(), self.endpoint(entry));
            checks.spawn(async move {
                let check = match asked {
                    Ok((url, key)) => probe.check(&url, key.as_ref()).await,
                    Err(reason) => Check::Fails { reason },
                };
                (at, check)
            });
        }
        let mut found: Vec<Option<Check>> = vec![None; entries.len()];
        while let Some(joined) = checks.join_next().await {
            if let Ok((at, check)) = joined {
                found[at] = Some(check);
            }
        }
        let stopped = || Check::Fails { reason: "the check stopped".into() };
        let checked = entries.into_iter().zip(found);
        checked
            .map(|(entry, check)| Indexer {
                key: entry.key.origin(),
                name: entry.name,
                url: entry.url,
                check: check.unwrap_or_else(stopped),
            })
            .collect()
    }

    /// Where to ask `entry`, or why it cannot be asked.
    fn endpoint(&self, entry: &IndexerEntry) -> Result<(Url, Option<ApiKey>), String> {
        let key = entry.key.resolve(&self.env);
        if let (Key::Env(var), None) = (&entry.key, &key) {
            return Err(format!("{var}, which holds its key, is not set for the engine"));
        }
        let url = Url::parse(&entry.url).map_err(|_| "its url is not a URL".to_string())?;
        Ok((url, key))
    }

    /// Writes `listed` to config.yaml, and searches it from now on. A config.yaml that fails to
    /// load as a whole keeps the old list.
    fn apply(&self, ctx: &Ctx, listed: &Listed, rerun: &str) -> Result<(), Error> {
        let before = Config::load(&self.paths, &self.env)?.search;
        let torznab = listed
            .entries
            .iter()
            .map(|entry| TorznabConfig {
                name: entry.name.clone(),
                url: entry.url.clone(),
                api_key: entry.key.resolve(&self.env).map(|key| key.expose().to_string()),
            })
            .collect();
        let after = SearchConfig { torznab, disabled: listed.disabled.clone(), ..before.clone() };
        ctx.search.reconfigure(&after)?;
        let writes = Writes::new(&self.env, rerun.to_string());
        if let Err(e) = config::save_indexers(&self.paths, listed, &writes) {
            let _ = ctx.search.reconfigure(&before);
            return Err(e);
        }
        Ok(())
    }
}

/// Every indexer in config.yaml, each asked now whether it answers.
pub async fn list(ctx: &Ctx) -> Result<IndexerList, Error> {
    let entries = config::indexers(&ctx.indexers.paths)?.entries;
    Ok(IndexerList { indexers: ctx.indexers.checked(entries).await, added: Vec::new() })
}

/// Asks `url` what it is, and writes each indexer it finds to config.yaml with `key`: one for a
/// Torznab endpoint, and each indexer of a Prowlarr or Jackett server. An entry with the same URL
/// takes the new key, and the name when one is given.
pub async fn add(ctx: &Ctx, url: &str, name: Option<String>, key: Option<ApiKey>) -> Result<IndexerList, Error> {
    let indexers = &ctx.indexers;
    let (url, key) = parse(url, key)?;
    if let Some(name) = &name {
        valid(name)?;
    }
    let wanted: Vec<(Option<String>, String, Url)> = match indexers.probe.find(&url, key.as_ref()).await? {
        Found::Endpoint { .. } => vec![(name, derived(&url), url.clone())],
        Found::Server { app, indexers: hosted } => {
            if name.is_some() && hosted.len() > 1 {
                let message = format!("{} at {url} has {} indexers, and --name names one", app.as_str(), hosted.len());
                return Err(Error::new(ErrorCode::Usage, message).with_hint("leave out --name"));
            }
            hosted.into_iter().map(|hosted| (name.clone(), slug(&hosted.name), hosted.url)).collect()
        }
    };
    let writing = indexers.writing.lock().await;
    let mut listed = config::indexers(&indexers.paths)?;
    let mut added = Vec::new();
    for (named, derived, url) in wanted {
        let at = listed.entries.iter().position(|entry| Url::parse(&entry.url).ok().as_ref() == Some(&url));
        let name = match named {
            Some(named) if taken(&listed, &named, at) => {
                return Err(Error::new(ErrorCode::Usage, format!("another source is named {named}"))
                    .with_hint("pick another --name; torrential indexers lists the names in use"));
            }
            Some(named) => named,
            None => match at {
                Some(at) => listed.entries[at].name.clone(),
                None => free(&listed, &derived),
            },
        };
        let entry = match at {
            Some(at) => &mut listed.entries[at],
            None => {
                listed.entries.push(IndexerEntry { name: String::new(), url: url.to_string(), key: Key::None });
                listed.entries.last_mut().expect("just pushed")
            }
        };
        entry.name.clone_from(&name);
        if let Some(key) = &key {
            entry.key = Key::Inline(key.clone());
        }
        added.push(name);
    }
    indexers.apply(ctx, &listed, "torrential indexers add")?;
    drop(writing);
    log(&format!("added the indexers {} from {url}", added.join(", ")));
    Ok(IndexerList { added, ..list(ctx).await? })
}

/// Takes the indexer named `name` out of config.yaml, and out of `sources.disabled`.
pub async fn remove(ctx: &Ctx, name: &str) -> Result<IndexerList, Error> {
    let indexers = &ctx.indexers;
    let writing = indexers.writing.lock().await;
    let mut listed = config::indexers(&indexers.paths)?;
    let before = listed.entries.len();
    listed.entries.retain(|entry| entry.name != name);
    if listed.entries.len() == before {
        return Err(Error::new(ErrorCode::NotFound, format!("no indexer is named {name:?}"))
            .with_hint("torrential indexers lists them"));
    }
    listed.disabled.retain(|disabled| disabled != name);
    indexers.apply(ctx, &listed, "torrential indexers remove")?;
    drop(writing);
    log(&format!("removed the indexer {name}"));
    list(ctx).await
}

/// `text` as an http or https URL with no API key in its query, and the key: `key`, else the
/// one the URL held.
fn parse(text: &str, key: Option<ApiKey>) -> Result<(Url, Option<ApiKey>), Error> {
    let mut url = Url::parse(text.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        .ok_or_else(|| {
            Error::new(ErrorCode::Usage, "the indexer's address is not an http or https URL")
                .with_hint("pass its address, such as http://127.0.0.1:9696 for Prowlarr")
        })?;
    let mut held = None;
    let mut kept = Vec::new();
    for (name, value) in url.query_pairs() {
        if name.eq_ignore_ascii_case("apikey") {
            held = ApiKey::new(&value);
        } else {
            kept.push((name.into_owned(), value.into_owned()));
        }
    }
    url.set_query(None);
    if !kept.is_empty() {
        url.query_pairs_mut().extend_pairs(kept);
    }
    Ok((url, key.or(held)))
}

/// A `--source` name: lowercase letters, digits, `-`, `_`, and `.`.
fn valid(name: &str) -> Result<(), Error> {
    let fits =
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b));
    if fits {
        return Ok(());
    }
    Err(Error::new(ErrorCode::Usage, format!("the name {name:?} is not a source name"))
        .with_hint("use lowercase letters, digits, '-', '_', or '.'"))
}

/// Whether a built-in source or an entry other than the one at `except` is named `name`.
fn taken(listed: &Listed, name: &str, except: Option<usize>) -> bool {
    torrential_search::builtin(name)
        || listed.entries.iter().enumerate().any(|(at, entry)| Some(at) != except && entry.name == name)
}

/// `name`, or the first of `name-2`, `name-3` and on that is free.
fn free(listed: &Listed, name: &str) -> String {
    let mut candidates = std::iter::once(name.to_string()).chain((2..).map(|n| format!("{name}-{n}")));
    candidates.find(|candidate| !taken(listed, candidate, None)).expect("names never run out")
}

/// A name for the endpoint at `url`: a Jackett indexer's ID, a Prowlarr indexer's number, or the host.
fn derived(url: &Url) -> String {
    let segments: Vec<&str> = url.path_segments().map(|s| s.filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    let jackett = segments.windows(3).find(|w| w[0] == "indexers" && w[2] == "results").map(|w| w[1]);
    let named = match (jackett, segments.as_slice()) {
        (Some("all"), _) => "jackett".to_string(),
        (Some(id), _) => id.to_string(),
        (None, [number, "api"]) if number.parse::<u32>().is_ok() => format!("prowlarr-{number}"),
        _ => url.host_str().unwrap_or_default().to_string(),
    };
    slug(&named)
}

/// `name` as a source name: lowercase, with a `-` for each run of other characters.
fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches(|c| c == '-' || c == '.');
    if slug.is_empty() { "indexer".into() } else { slug.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_indexer_the_address_names() {
        let named = |url: &str| derived(&Url::parse(url).unwrap());
        assert_eq!(named("http://127.0.0.1:9117/api/v2.0/indexers/filelist/results/torznab/api"), "filelist");
        assert_eq!(named("http://127.0.0.1:9117/api/v2.0/indexers/all/results/torznab/"), "jackett");
        assert_eq!(named("http://nas.local:9696/prowlarr/12/api"), "nas.local");
        assert_eq!(named("http://127.0.0.1:9696/12/api"), "prowlarr-12");
        assert_eq!(named("http://bitmagnet.lan:3333/torznab"), "bitmagnet.lan");
        assert_eq!(
            [slug("The Pirate Bay"), slug("FileList.io"), slug("Ро́ (RO)"), slug("!!")],
            ["the-pirate-bay", "filelist.io", "ro", "indexer"]
        );
    }

    #[test]
    fn a_key_in_the_address_moves_out_of_it() {
        let (url, key) = parse("http://127.0.0.1:9696/3/api?apikey=k3y&extended=1", None).unwrap();
        assert_eq!(
            (url.as_str(), key.as_ref().map(ApiKey::expose)),
            ("http://127.0.0.1:9696/3/api?extended=1", Some("k3y"))
        );
        let (url, key) = parse("http://127.0.0.1:9696/3/api?apikey=old", ApiKey::new("new")).unwrap();
        assert_eq!((url.as_str(), key.as_ref().map(ApiKey::expose)), ("http://127.0.0.1:9696/3/api", Some("new")));
        let refused = parse("ftp://127.0.0.1/?apikey=k3y", None).unwrap_err();
        assert!(!refused.message.contains("k3y"), "{}", refused.message);
    }
}
