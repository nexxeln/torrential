use tokio::sync::mpsc;
use torrential_core::{Error, HexPrefix, Profile, Release, ReleaseId, SearchQuery, SearchResult};
use torrential_search::Search;
pub use torrential_search::{Progress, SavedLink};

use super::BoxFuture;

pub type RankFor<'a> = &'a (dyn Fn() -> Profile + Send + Sync);

pub trait SearchPort: Send + Sync {
    /// Searches every source, sending `progress` as each answers. A caller that does not watch
    /// passes a sender whose receiver is gone.
    fn search_stream<'a>(
        &'a self,
        query: SearchQuery,
        rank_for: RankFor<'a>,
        progress: mpsc::UnboundedSender<Progress>,
    ) -> BoxFuture<'a, Result<SearchResult, Error>>;
    fn fetch_torrent(&self, url: String) -> BoxFuture<'_, Result<Vec<u8>, Error>>;
    fn find(&self, prefix: &HexPrefix) -> Vec<Release>;
    fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error>;
    fn saved_link(&self, id: &ReleaseId) -> Option<SavedLink>;
    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>);
}

impl SearchPort for Search {
    fn search_stream<'a>(
        &'a self,
        query: SearchQuery,
        rank_for: RankFor<'a>,
        progress: mpsc::UnboundedSender<Progress>,
    ) -> BoxFuture<'a, Result<SearchResult, Error>> {
        Box::pin(async move {
            self.search_with_progress(&query, rank_for, |update| {
                let _ = progress.send(update);
            })
            .await
        })
    }

    fn fetch_torrent(&self, url: String) -> BoxFuture<'_, Result<Vec<u8>, Error>> {
        Box::pin(async move { Search::fetch_torrent(self, &url).await })
    }

    fn find(&self, prefix: &HexPrefix) -> Vec<Release> {
        self.cache().find(prefix)
    }

    fn torrent_url(&self, release: &Release) -> Result<Option<String>, Error> {
        Search::torrent_url(self, release)
    }

    fn saved_link(&self, id: &ReleaseId) -> Option<SavedLink> {
        self.cache().saved_link(id)
    }

    fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>) {
        self.cache().restore(saved);
    }
}
