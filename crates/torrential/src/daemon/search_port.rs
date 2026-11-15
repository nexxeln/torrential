use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tokio::sync::mpsc;
use torrential_core::{Error, HexPrefix, Profile, Release, ReleaseId, SearchQuery, SearchResult};
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
    /// Looks up web seeds that serve every file of a torrent. `file` is one file of a torrent
    /// with several, as a path that starts with the torrent's name.
    fn mirrors(
        &self,
        name: &str,
        file: Option<&Path>,
        web_seeds: &[String],
    ) -> Option<BoxFuture<'static, Result<Vec<String>, Error>>>;
    fn find(&self, prefix: &HexPrefix) -> Vec<Release>;
    fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error>;
    fn saved_link(&self, id: &ReleaseId) -> Option<SavedLink>;
    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>);
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

    fn mirrors(
        &self,
        name: &str,
        file: Option<&Path>,
        web_seeds: &[String],
    ) -> Option<BoxFuture<'static, Result<Vec<String>, Error>>> {
        Some(Box::pin(self.search.mirrors(name, file, web_seeds)?))
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
}
