use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use torrential_core::{
    Error, ErrorCode, HexPrefix, Profile, Release, ReleaseId, SearchQuery, SearchResult, SourceOutcome, SourceReport,
};
use torrential_search::rank;

use crate::daemon::search_port::{BoxFuture, Progress, RankFor, SavedLink, SearchPort};

/// A search that answers from a fixed catalog, each source after its own delay.
#[derive(Clone, Default)]
pub struct Search(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    catalog: Vec<Release>,
    torrents: BTreeMap<String, Vec<u8>>,
    delay: Duration,
    script: Vec<Source>,
    recent: Vec<Release>,
    searches: usize,
    /// The profile the last answer was ranked for.
    ranked_for: Option<Profile>,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub after: Duration,
    pub failure: Option<SourceOutcome>,
}

impl Source {
    pub fn answers(name: &str, after_ms: u64) -> Self {
        Self { name: name.into(), after: Duration::from_millis(after_ms), failure: None }
    }

    pub fn fails(name: &str, after_ms: u64, failure: SourceOutcome) -> Self {
        Self { name: name.into(), after: Duration::from_millis(after_ms), failure: Some(failure) }
    }
}

impl Search {
    #[cfg(test)]
    pub fn new(catalog: Vec<Release>) -> Self {
        Self::scripted(catalog, vec![Source::answers("fake", 0)], BTreeMap::new())
    }

    pub fn scripted(catalog: Vec<Release>, mut script: Vec<Source>, torrents: BTreeMap<String, Vec<u8>>) -> Self {
        script.sort_by_key(|s| s.after);
        Self(Arc::new(Mutex::new(State {
            catalog,
            torrents,
            delay: Duration::ZERO,
            script,
            recent: Vec::new(),
            searches: 0,
            ranked_for: None,
        })))
    }

    #[cfg(test)]
    pub fn searches(&self) -> usize {
        self.0.lock().unwrap().searches
    }

    #[cfg(test)]
    pub fn ranked_for(&self) -> Option<Profile> {
        self.0.lock().unwrap().ranked_for.clone()
    }

    #[cfg(test)]
    pub fn serve_torrent(&self, url: &str, bytes: Vec<u8>) {
        self.0.lock().unwrap().torrents.insert(url.to_string(), bytes);
    }

    #[cfg(test)]
    pub fn set_delay(&self, delay: Duration) {
        self.0.lock().unwrap().delay = delay;
    }

    fn parts(&self) -> (Vec<Release>, Vec<Source>, Duration) {
        let mut state = self.0.lock().unwrap();
        state.searches += 1;
        (state.catalog.clone(), state.script.clone(), state.delay)
    }

    fn remember(&self, result: SearchResult) -> SearchResult {
        let mut state = self.0.lock().unwrap();
        state.recent.retain(|r| !result.releases.iter().any(|n| n.id == r.id));
        state.recent.splice(0..0, result.releases.iter().cloned());
        state.ranked_for = Some(result.query.profile.clone());
        result
    }
}

fn compose(
    catalog: &[Release],
    answered: &[Source],
    query: SearchQuery,
    profile: Profile,
    elapsed: Duration,
) -> SearchResult {
    let words: Vec<String> = query.text.split_whitespace().map(str::to_lowercase).collect();
    // A title's search keeps its releases in the catalog's order, each with what of it it holds
    // and how its swarm stands, and drops a dead swarm as the real search does.
    let found = |r: &Release| match &query.wanted {
        Some(wanted) => {
            let span = Some(wanted.fit(&r.name)?.span);
            let swarm = Some(rank::swarm(r.seeders, r.sources.iter().any(|s| torrential_search::lawful(s)))?);
            Some(Release { span, swarm, ..r.clone() })
        }
        None => words.iter().all(|w| r.name.to_lowercase().contains(w.as_str())).then(|| r.clone()),
    };
    let matches = |r: &Release| found(r).is_some();
    let listed_by = |r: &Release, source: &Source| r.sources.first() == Some(&source.name);
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

impl SearchPort for Search {
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

    fn mirrors(
        &self,
        _: &str,
        _: Option<&Path>,
        _: &[String],
    ) -> Option<BoxFuture<'static, Result<Vec<String>, Error>>> {
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
}
