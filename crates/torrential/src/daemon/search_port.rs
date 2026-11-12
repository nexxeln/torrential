use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use torrential_core::{
    Error, ErrorCode, HexPrefix, Profile, Release, ReleaseId, SearchQuery, SearchResult, SourceOutcome, SourceReport,
};
pub use torrential_search::{Progress, SavedLink};
use torrential_search::{ResultCache, Search, SearchConfig};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type RankFor<'a> = &'a (dyn Fn() -> Profile + Send + Sync);

pub trait SearchPort: Send + Sync {
    fn search<'a>(&'a self, query: SearchQuery, rank_for: RankFor<'a>) -> BoxFuture<'a, Result<SearchResult, Error>>;
    fn search_stream<'a>(
        &'a self,
        query: SearchQuery,
        rank_for: RankFor<'a>,
        progress: mpsc::UnboundedSender<Progress>,
    ) -> BoxFuture<'a, Result<SearchResult, Error>>;
    fn fetch_torrent(&self, url: String) -> BoxFuture<'_, Result<Vec<u8>, Error>>;
    fn mirrors(&self, name: &str, web_seeds: &[String]) -> Option<BoxFuture<'static, Result<Vec<String>, Error>>>;
    fn find(&self, prefix: &HexPrefix) -> Vec<Release>;
    fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error>;
    fn saved_link(&self, id: &ReleaseId) -> Option<SavedLink>;
    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>);
    fn answers_expire(&self) -> Option<Instant>;
}

pub struct Live {
    search: Search,
    cache: ResultCache,
}

impl Live {
    pub fn new(config: &SearchConfig) -> Result<Self, Error> {
        let cache = ResultCache::new();
        Ok(Self { search: Search::new(config, cache.clone())?, cache })
    }
}

impl SearchPort for Live {
    fn search<'a>(&'a self, query: SearchQuery, rank_for: RankFor<'a>) -> BoxFuture<'a, Result<SearchResult, Error>> {
        Box::pin(async move { self.search.search_with_progress(&query, rank_for, |_| {}).await })
    }

    fn search_stream<'a>(
        &'a self,
        query: SearchQuery,
        rank_for: RankFor<'a>,
        progress: mpsc::UnboundedSender<Progress>,
    ) -> BoxFuture<'a, Result<SearchResult, Error>> {
        Box::pin(async move {
            self.search
                .search_with_progress(&query, rank_for, |update| {
                    let _ = progress.send(update);
                })
                .await
        })
    }

    fn fetch_torrent(&self, url: String) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move { self.search.fetch_torrent(&url).await })
    }

    fn mirrors(&self, name: &str, web_seeds: &[String]) -> Option<BoxFuture<'static, Result<Vec<String>, Error>>> {
        Some(Box::pin(self.search.mirrors(name, web_seeds)?))
    }

    fn find(&self, prefix: &HexPrefix) -> Vec<Release> {
        self.cache.find(prefix)
    }

    fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error> {
        self.search.torrent_url(release)
    }

    fn saved_link(&self, id: &ReleaseId) -> Option<SavedLink> {
        self.cache.saved_link(id)
    }

    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>) {
        self.cache.restore(saved);
    }

    fn answers_expire(&self) -> Option<Instant> {
        self.cache.expires()
    }
}

#[derive(Clone, Default)]
pub struct Fake(Arc<Mutex<FakeState>>);

#[derive(Default)]
struct FakeState {
    catalog: Vec<Release>,
    torrents: BTreeMap<String, Vec<u8>>,
    delay: Duration,
    script: Vec<FakeSource>,
    recent: Vec<Release>,
}

#[derive(Clone, Debug)]
pub struct FakeSource {
    pub name: String,
    pub after: Duration,
    pub failure: Option<SourceOutcome>,
}

impl FakeSource {
    pub fn answers(name: &str, after_ms: u64) -> Self {
        Self { name: name.into(), after: Duration::from_millis(after_ms), failure: None }
    }

    pub fn fails(name: &str, after_ms: u64, failure: SourceOutcome) -> Self {
        Self { name: name.into(), after: Duration::from_millis(after_ms), failure: Some(failure) }
    }
}

impl Fake {
    #[cfg(test)]
    pub fn new(catalog: Vec<Release>) -> Self {
        Self::scripted(catalog, vec![FakeSource::answers("fake", 0)], BTreeMap::new())
    }

    pub fn scripted(catalog: Vec<Release>, mut script: Vec<FakeSource>, torrents: BTreeMap<String, Vec<u8>>) -> Self {
        script.sort_by_key(|s| s.after);
        Self(Arc::new(Mutex::new(FakeState { catalog, torrents, delay: Duration::ZERO, script, recent: Vec::new() })))
    }

    #[cfg(test)]
    pub fn serve_torrent(&self, url: &str, bytes: Vec<u8>) {
        self.0.lock().unwrap().torrents.insert(url.to_string(), bytes);
    }

    #[cfg(test)]
    pub fn set_delay(&self, delay: Duration) {
        self.0.lock().unwrap().delay = delay;
    }

    fn parts(&self) -> (Vec<Release>, Vec<FakeSource>, Duration) {
        let state = self.0.lock().unwrap();
        (state.catalog.clone(), state.script.clone(), state.delay)
    }

    fn remember(&self, result: SearchResult) -> SearchResult {
        let mut state = self.0.lock().unwrap();
        state.recent.retain(|r| !result.releases.iter().any(|n| n.id == r.id));
        state.recent.splice(0..0, result.releases.iter().cloned());
        result
    }
}

pub fn fake_catalog() -> Vec<Release> {
    let release = |hex: &str, name: &str, size: u64, seeders: u32| Release {
        id: torrential_core::ReleaseId::Torrent(hex.parse().expect("catalog IDs are valid")),
        name: name.to_string(),
        size: Some(size),
        seeders: Some(seeders),
        leechers: None,
        published: None,
        category: Some(torrential_core::Category::Software),
        flags: Default::default(),
        sources: vec!["fake".into()],
        magnet: Some(format!("magnet:?xt=urn:btih:{hex}&dn={name}&xl={size}")),
        torrent_url: None,
        info_url: None,
        span: None,
    };
    vec![
        release("9b521219bc48550140a666f74a29048ed78e933b", "archlinux-2026.10.01-x86_64.iso", 1_640_497_152, 412),
        release("e3a1f06d2b1c8a4f5e6d7c8b9a0f1e2d3c4b5a69", "ubuntu-24.04.5-desktop-amd64.iso", 6_345_887_744, 1873),
    ]
}

fn compose(
    catalog: &[Release],
    answered: &[FakeSource],
    query: SearchQuery,
    profile: Profile,
    elapsed: Duration,
) -> SearchResult {
    let words: Vec<String> = query.text.split_whitespace().map(str::to_lowercase).collect();
    // A title's search keeps its releases in the catalog's order, each with what of it it holds.
    let found = |r: &Release| match &query.wanted {
        Some(wanted) => wanted.fit(&r.name).map(|named| Release { span: Some(named.span), ..r.clone() }),
        None => words.iter().all(|w| r.name.to_lowercase().contains(w.as_str())).then(|| r.clone()),
    };
    let matches = |r: &Release| found(r).is_some();
    let listed_by = |r: &Release, source: &FakeSource| r.sources.first() == Some(&source.name);
    let sources = answered
        .iter()
        .map(|source| {
            let outcome = source.failure.clone().unwrap_or_else(|| SourceOutcome::Ok {
                results: catalog.iter().filter(|r| listed_by(r, source) && matches(r)).count() as u32,
            });
            SourceReport { source: source.name.clone(), outcome, elapsed_ms: source.after.as_millis() as u32 }
        })
        .collect();
    let matching: Vec<Release> = catalog
        .iter()
        .filter(|r| answered.iter().any(|s| s.failure.is_none() && listed_by(r, s)))
        .filter_map(found)
        .collect();
    let total = matching.len() as u32;
    let releases = matching.into_iter().take(query.limit as usize).collect();
    let query = SearchQuery { profile, ..query };
    SearchResult { query, releases, total, hidden_adult: 0, sources, elapsed_ms: elapsed.as_millis() as u32 }
}

impl SearchPort for Fake {
    fn search<'a>(&'a self, query: SearchQuery, rank_for: RankFor<'a>) -> BoxFuture<'a, Result<SearchResult, Error>> {
        Box::pin(async move {
            let (catalog, script, delay) = self.parts();
            tokio::time::sleep(delay).await;
            Ok(self.remember(compose(&catalog, &script, query, rank_for(), delay)))
        })
    }

    fn search_stream<'a>(
        &'a self,
        query: SearchQuery,
        rank_for: RankFor<'a>,
        progress: mpsc::UnboundedSender<Progress>,
    ) -> BoxFuture<'a, Result<SearchResult, Error>> {
        Box::pin(async move {
            let (catalog, script, _) = self.parts();
            let started = tokio::time::Instant::now();
            for answered in 1..script.len() {
                tokio::time::sleep_until(started + script[answered - 1].after).await;
                let partial = compose(&catalog, &script[..answered], query.clone(), rank_for(), started.elapsed());
                let result = self.remember(partial);
                let pending = script[answered..].iter().map(|s| s.name.clone()).collect();
                let _ = progress.send(Progress { result, pending });
            }
            if let Some(last) = script.last() {
                tokio::time::sleep_until(started + last.after).await;
            }
            Ok(self.remember(compose(&catalog, &script, query, rank_for(), started.elapsed())))
        })
    }

    fn fetch_torrent(&self, url: String) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move {
            self.0.lock().unwrap().torrents.get(&url).cloned().ok_or_else(|| {
                Error::new(ErrorCode::FetchFailed, "could not fetch the .torrent file: HTTP 404")
                    .with_hint("try the magnet link instead")
            })
        })
    }

    fn mirrors(&self, _: &str, _: &[String]) -> Option<BoxFuture<'static, Result<Vec<String>, Error>>> {
        None
    }

    fn find(&self, prefix: &HexPrefix) -> Vec<Release> {
        self.0.lock().unwrap().recent.iter().filter(|r| prefix.matches(&r.id)).cloned().collect()
    }

    fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error> {
        Ok(release.torrent_url.clone())
    }

    fn saved_link(&self, _: &ReleaseId) -> Option<SavedLink> {
        None
    }

    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>) {
        let mut state = self.0.lock().unwrap();
        for (release, _) in saved {
            if !state.recent.iter().any(|r| r.id == release.id) {
                state.recent.push(release);
            }
        }
    }

    fn answers_expire(&self) -> Option<Instant> {
        None
    }
}
