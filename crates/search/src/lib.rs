//! Turns a [`SearchQuery`] into a [`SearchResult`]: every selected source runs at
//! once under the query's deadline, listings merge by torrent, and one ranking
//! orders them, so a human and an agent see the same list.
//!
//! The engine process owns one [`Search`] and one [`ResultCache`]. Search never
//! starts a download.

mod assemble;
mod cache;
pub mod catalog;
mod classify;
mod dates;
mod http;
mod imdb;
pub mod indexers;
mod metainfo;
mod observation;
pub mod rank;
mod redirect;
mod sources;
mod store;
pub mod subtitles;
mod terms;
mod web_seeds;

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::future::{BoxFuture, Shared};
use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, timeout_at};
use torrential_core::{Error, ErrorCode, Profile, Release, SearchQuery, SearchResult, text};
use url::Url;

use crate::assemble::{Answer, Linked, Run, assemble, merged, vet};
use crate::cache::AnswerKey;
pub use crate::cache::ResultCache;
use crate::http::{FetchError, Http, Retry};
use crate::sources::{Ctx, Failure, Source, Stores, archive_org};
pub use crate::sources::{SavedLink, builtin, lawful};
use crate::terms::Terms;
pub use crate::web_seeds::WebSeeds;

#[derive(Clone, PartialEq, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Where the Academic Torrents catalog, the official listing pages, and hashed
    /// `.torrent` files are kept between processes. `None` writes nothing to disk.
    pub cache_dir: Option<PathBuf>,
    /// Searched after the built-in sources, in this order.
    pub torznab: Vec<TorznabConfig>,
    /// Replacement base URLs for built-in sources, by source name, such as a
    /// mirror. torrential never adds mirrors on its own.
    pub endpoints: BTreeMap<String, String>,
    /// Sources never searched, by name. A query that names one fails.
    pub disabled: Vec<String>,
}

/// A Jackett, Prowlarr, or bitmagnet Torznab endpoint, such as
/// `http://127.0.0.1:9117/api/v2.0/indexers/all/results/torznab/api`.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TorznabConfig {
    /// The `--source` name: lowercase letters, digits, `-`, `_`, and `.`.
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

impl fmt::Debug for TorznabConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TorznabConfig")
            .field("name", &self.name)
            .field("url", &self.url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct SourceInfo {
    pub name: String,
    /// A catalog of content its publishers share, ranked ahead of general indexes.
    pub lawful: bool,
}

/// A search under way, with answers merged and ranked like the final result.
/// Pending sources follow the query's source order, or authority order when it
/// names none.
#[derive(Clone, PartialEq, Debug)]
pub struct Progress {
    pub result: SearchResult,
    pub pending: Vec<String>,
}

#[derive(Clone)]
pub struct Search {
    inner: Arc<Inner>,
}

struct Inner {
    http: Http,
    /// For the redirects of a torrent's web seeds, which must not lead inside.
    seeds: Http,
    /// The sources a search may use, which [`Search::reconfigure`] replaces whole.
    registry: RwLock<Arc<Registry>>,
    stores: Stores,
    cache: ResultCache,
    /// archive.org's answers about an item's servers, asked for as its `.torrent` file was fetched,
    /// so a download of it starts from those servers.
    servers: Mutex<VecDeque<Servers>>,
}

struct Registry {
    enabled: Vec<Arc<Source>>,
    disabled: Vec<String>,
}

impl Registry {
    fn new(config: &SearchConfig) -> Result<Registry, Error> {
        Ok(Registry { enabled: sources::registry(config)?, disabled: config.disabled.clone() })
    }
}

type Lookup = Shared<BoxFuture<'static, Result<Vec<String>, Error>>>;

/// One item's lookup of its servers, begun at `asked`.
struct Servers {
    identifier: String,
    asked: Instant,
    lookup: Lookup,
}

/// Lookups begun with a `.torrent` fetch whose download has not started yet.
const SERVERS_KEPT: usize = 8;
/// How long a lookup's answer is used for a download that starts after it.
const SERVERS_FOR: Duration = Duration::from_secs(60);

impl Search {
    /// Fails with `usage` when the config names an unknown source or a bad URL.
    pub fn new(config: &SearchConfig, cache: ResultCache) -> Result<Search, Error> {
        let http = Http::new().map_err(no_client)?;
        Inner::new(config, cache, http).map(|inner| Search { inner: Arc::new(inner) })
    }

    /// The results this search keeps for later lookups by ID.
    pub fn cache(&self) -> &ResultCache {
        &self.inner.cache
    }

    /// The web-seed lookup for downloads. It shares this search's lookups, so the servers of an
    /// archive.org item asked for with its `.torrent` are not asked for again.
    pub fn web_seeds(&self) -> WebSeeds {
        WebSeeds(self.inner.clone())
    }

    /// Every enabled source, most authoritative first.
    pub fn sources(&self) -> Vec<SourceInfo> {
        let registry = self.inner.registry();
        registry.enabled.iter().map(|s| SourceInfo { name: s.meta.name.clone(), lawful: s.meta.lawful }).collect()
    }

    /// Searches the Torznab indexers and leaves out the sources `config` names from now on. Searches
    /// under way finish with the sources they started with. Fails with `usage`, and changes nothing,
    /// when the config names an unknown source or a bad URL.
    pub fn reconfigure(&self, config: &SearchConfig) -> Result<(), Error> {
        let registry = Arc::new(Registry::new(config)?);
        *self.inner.registry.write().unwrap_or_else(|e| e.into_inner()) = registry;
        Ok(())
    }

    /// Returns by the query's deadline, ranked for the query's profile. A source that has
    /// not answered by then is reported as `timeout`. Fails with `providers_unavailable`
    /// when no source answered, and with `usage` for a query without words or an unknown
    /// source.
    pub async fn search(&self, query: &SearchQuery) -> Result<SearchResult, Error> {
        self.search_with_progress(query, || query.profile.clone(), |_| {}).await
    }

    /// Like [`Search::search`], ranked for what `rank_for` returns each time results
    /// change, so a profile that becomes known during the search applies. Calls
    /// `progress` each time sources answer while others are still pending. Answers that
    /// arrive together make one call. A query with a wanted title searches each of the
    /// title's queries at every source, and a source is pending until all have answered.
    pub async fn search_with_progress(
        &self,
        query: &SearchQuery,
        rank_for: impl Fn() -> Profile + Send,
        mut progress: impl FnMut(Progress) + Send,
    ) -> Result<SearchResult, Error> {
        let started = Instant::now();
        let texts: Vec<Terms> = match &query.wanted {
            Some(wanted) => wanted.queries().iter().map(|text| Terms::parse(text)).collect(),
            None => vec![Terms::parse(&query.text)],
        };
        if texts.iter().any(Terms::is_empty) {
            return Err(
                Error::new(ErrorCode::Usage, "a search needs at least one word").with_hint("torrential search <words>")
            );
        }
        // The first query's words weigh relevance, since it names the title most fully.
        let terms = &texts[0];
        let deadline = started + Duration::from_millis(query.timeout_ms.into());
        let cache = &self.inner.cache;
        let mut selected = self.select(&query.sources)?;
        if query.wanted.is_some() {
            selected.retain(|source| source.video);
        }
        let lookups = selected.iter().flat_map(|source| texts.iter().map(move |terms| (source, terms))).enumerate();
        let lookups = lookups.map(|(index, (source, terms))| {
            let key = AnswerKey {
                source: source.meta.name.clone(),
                scope: source.scope(),
                text: terms.text().to_string(),
                adult: query.adult,
                title: query.wanted.is_some(),
            };
            let cached = if query.fresh { None } else { cache.answer(&key, std::time::Instant::now()) };
            async move {
                if let Some(rows) = cached {
                    return (index, Run { source: source.meta.clone(), answer: Answer::Cached(rows), elapsed_ms: 0 });
                }
                let (answer, complete) = AssertUnwindSafe(self.inner.run(source, terms, query, started, deadline))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| {
                        (Answer::Failed(Failure::BadResponse("the source adapter crashed".into())), false)
                    });
                if let Answer::Fresh(rows) = &answer
                    && complete
                {
                    cache.store(key, rows.clone(), std::time::Instant::now());
                }
                (index, Run { source: source.meta.clone(), answer, elapsed_ms: millis(started.elapsed()) })
            }
        });
        let mut lookups: FuturesUnordered<_> = lookups.collect();
        // By source, then by text, so runs merge in the same order however they finish.
        let mut runs: Vec<Option<Run>> = vec![None; selected.len() * texts.len()];
        while let Some((index, run)) = lookups.next().await {
            runs[index] = Some(run);
            while let Some(Some((index, run))) = lookups.next().now_or_never() {
                runs[index] = Some(run);
            }
            if !lookups.is_empty() {
                let answered: Vec<Run> = runs.iter().flatten().cloned().collect();
                let (result, linked) = merged(query, rank_for(), terms, &answered, millis(started.elapsed()));
                cache.remember(self.inner.saved_links(linked));
                let pending = selected
                    .iter()
                    .zip(runs.chunks(texts.len()))
                    .filter(|(_, runs)| runs.iter().any(Option::is_none))
                    .map(|(s, _)| s.meta.name.clone())
                    .collect();
                progress(Progress { result, pending });
            }
        }
        self.inner.stores.save().await;
        let runs: Vec<Run> = runs.into_iter().flatten().collect();
        let (result, linked) = assemble(query, rank_for(), terms, &runs, millis(started.elapsed()))?;
        cache.remember(self.inner.saved_links(linked));
        Ok(result)
    }

    /// The `.torrent` link to download `release` from: its own, or the one an earlier engine
    /// process saved for it, with the current API key of the source that issued it. Fails with
    /// `usage` when that link needs a key and its source is gone or has none.
    pub fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error> {
        if release.torrent_url.is_some() {
            return Ok(release.torrent_url.clone());
        }
        let Some(saved) = self.inner.cache.saved_link(&release.id) else { return Ok(None) };
        let name = &saved.source;
        let needs_key = |why: &str, hint: String| {
            Error::new(
                ErrorCode::Usage,
                format!("this result's .torrent link needs an API key for the {name} indexer, which {why}"),
            )
            .with_hint(format!("{hint}, or search again"))
        };
        let registry = self.inner.registry();
        let Some(source) = registry.enabled.iter().find(|s| s.meta.name == *name) else {
            return Err(if registry.disabled.contains(name) {
                let hint = format!(
                    "remove {name} from sources.disabled in the torrential config and run torrential engine stop"
                );
                needs_key("is disabled", hint)
            } else {
                needs_key("is not configured", format!("add {name} back with torrential indexers add"))
            });
        };
        let url = source.link(&saved).ok_or_else(|| {
            needs_key("has none", format!("give {name} its key with torrential indexers add <its address> --key <key>"))
        })?;
        Ok(Some(url))
    }

    /// The `.torrent` file at `url`, which may be a private link from
    /// [`ResultCache::find`], so errors never quote it.
    pub async fn fetch_torrent(&self, url: &str) -> Result<Vec<u8>, Error> {
        const CAP: usize = 16 << 20;
        const WITHIN: Duration = Duration::from_secs(20);
        let url = Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| Error::new(ErrorCode::InvalidRef, "a .torrent URL must start with http:// or https://"))?;
        if let Some(item) = self.inner.registry().enabled.iter().find_map(|s| s.archive_item_of_link(&url)) {
            self.inner.ask_servers(item);
        }
        // A private tracker may count each request for a .torrent as a grab.
        let body = self.inner.http.fetch(&url, CAP, Instant::now() + WITHIN, Retry::Never).await;
        body.map_err(|e| {
            Error::new(ErrorCode::FetchFailed, format!("could not fetch the .torrent file: {}", fetch_reason(e)))
                .with_hint("check the network connection, or download by magnet link")
        })
    }

    fn select(&self, names: &[String]) -> Result<Vec<Arc<Source>>, Error> {
        let registry = self.inner.registry();
        let all = &registry.enabled;
        if names.is_empty() {
            return Ok(all.clone());
        }
        let mut chosen: Vec<Arc<Source>> = Vec::new();
        for name in names {
            let Some(source) = all.iter().find(|s| s.meta.name.eq_ignore_ascii_case(name.trim())) else {
                if let Some(off) = registry.disabled.iter().find(|d| d.eq_ignore_ascii_case(name.trim())) {
                    return Err(Error::new(ErrorCode::Usage, format!("source {off:?} is disabled"))
                        .with_hint("remove it from sources.disabled in the torrential config"));
                }
                let known: Vec<&str> = all.iter().map(|s| s.meta.name.as_str()).collect();
                return Err(Error::new(ErrorCode::Usage, format!("unknown source {name:?}"))
                    .with_hint(format!("available sources: {}", known.join(", "))));
            };
            if !chosen.iter().any(|c| Arc::ptr_eq(c, source)) {
                chosen.push(source.clone());
            }
        }
        Ok(chosen)
    }
}

impl Inner {
    fn new(config: &SearchConfig, cache: ResultCache, http: Http) -> Result<Inner, Error> {
        let seeds = Http::public(http::USER_AGENT, redirect::policy()).map_err(no_client)?;
        let registry = RwLock::new(Arc::new(Registry::new(config)?));
        let stores = Stores::new(config.cache_dir.clone());
        Ok(Inner { http, seeds, registry, stores, cache, servers: Mutex::default() })
    }

    fn registry(&self) -> Arc<Registry> {
        self.registry.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Starts asking archive.org for `item`'s servers, for a download of it that may start soon.
    fn ask_servers(&self, item: archive_org::Item) {
        let identifier = item.identifier().to_string();
        let lookup = servers(&self.http, item);
        let mut kept = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|s| s.identifier != identifier);
        if kept.len() == SERVERS_KEPT {
            kept.pop_front();
        }
        kept.push_back(Servers { identifier, asked: Instant::now(), lookup });
    }

    /// The lookup of `item`'s servers begun lately with its `.torrent` fetch, taken so a later
    /// download asks again.
    fn asked_servers(&self, item: &archive_org::Item) -> Option<Lookup> {
        let mut kept = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        let at = kept.iter().position(|s| s.identifier == item.identifier())?;
        let taken = kept.remove(at)?;
        (taken.asked.elapsed() < SERVERS_FOR).then_some(taken.lookup)
    }

    fn saved_links(&self, linked: Vec<Linked>) -> Vec<(Release, Option<SavedLink>)> {
        let registry = self.registry();
        linked
            .into_iter()
            .map(|(release, private)| {
                let saved = private.and_then(|name| {
                    let source = registry.enabled.iter().find(|s| s.meta.name == name)?;
                    source.save_link(release.torrent_url.as_deref()?)
                });
                (release, saved)
            })
            .collect()
    }

    async fn run(
        &self,
        source: &Source,
        terms: &Terms,
        query: &SearchQuery,
        started: Instant,
        deadline: Instant,
    ) -> (Answer, bool) {
        let ctx = Ctx {
            http: &self.http,
            stores: &self.stores,
            terms,
            adult: query.adult,
            fresh: query.fresh,
            wanted: query.wanted.as_ref(),
            started,
            deadline,
            partial: AtomicBool::new(false),
        };
        let rows = timeout_at(deadline, source.search(&ctx)).await.unwrap_or(Err(Failure::Timeout));
        let answer = match rows.and_then(|rows| vet(terms, rows)) {
            Ok(rows) => Answer::Fresh(Arc::new(rows)),
            Err(failure) => Answer::Failed(failure),
        };
        (answer, !ctx.partial.load(Ordering::Relaxed))
    }
}

fn fetch_reason(e: FetchError) -> String {
    match e {
        FetchError::Timeout(waited) => match waited.as_secs_f64().round() as u64 {
            1 => "no response in 1 second".to_string(),
            seconds => format!("no response in {seconds} seconds"),
        },
        FetchError::Status(status) => format!("HTTP {status}"),
        FetchError::Connect(reason) | FetchError::Decode(reason) => text::truncate(&text::sanitize(&reason), 160),
        FetchError::TooLarge(cap) => format!("larger than {} MiB", cap >> 20),
    }
}

fn millis(d: Duration) -> u32 {
    u32::try_from(d.as_millis()).unwrap_or(u32::MAX)
}

/// Asks archive.org for `item`'s servers on a task of its own, so the answer comes however long the
/// caller takes to wait for it.
fn servers(http: &Http, item: archive_org::Item) -> Lookup {
    let http = http.detached();
    let task = tokio::spawn(async move { item.mirrors(&http).await });
    let joined = task.map(|joined| joined.unwrap_or_else(|e| Err(Error::internal(format!("the lookup stopped: {e}")))));
    joined.boxed().shared()
}

fn no_client(e: reqwest::Error) -> Error {
    Error::internal(format!("cannot build the HTTP client: {e}"))
}

/// Runs one source the way a search does, with a 60-second deadline, and returns
/// every response body it received by URL, plus how the source fared. The
/// `record` example writes these as test fixtures.
#[doc(hidden)]
pub async fn record(config: &SearchConfig, source: &str, query: &str) -> Result<(String, http::Recorded), Error> {
    let inner = Inner::new(config, ResultCache::new(), Http::recording().map_err(no_client)?)?;
    let registry = inner.registry();
    let source = registry
        .enabled
        .iter()
        .find(|s| s.meta.name == source)
        .ok_or_else(|| Error::new(ErrorCode::Usage, format!("unknown source {source:?}")))?;
    let terms = Terms::parse(query);
    let ctx = Ctx {
        http: &inner.http,
        stores: &inner.stores,
        terms: &terms,
        adult: false,
        fresh: true,
        wanted: None,
        started: Instant::now(),
        deadline: Instant::now() + Duration::from_secs(60),
        partial: AtomicBool::new(false),
    };
    let outcome = match source.search(&ctx).await {
        Ok(rows) => format!("{} rows", rows.len()),
        Err(failure) => format!("{failure:?}"),
    };
    Ok((outcome, inner.http.take_recorded()))
}
