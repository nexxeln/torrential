use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use torrential_core::{HexPrefix, Release, ReleaseId};

use crate::observation::Observation;
use crate::sources::SavedLink;

/// Knaben's CDN caches searches for the same five minutes.
const TTL: Duration = Duration::from_secs(5 * 60);
const MAX_ANSWERS: usize = 512;
const MAX_RECENT: usize = 2048;

/// Each source's answer to a query, kept for five minutes, and every release a
/// search returned, so a short ID from a search resolves later.
///
/// Answers are kept per source and before filtering: a later search with the same
/// words reuses every source that answered, applies its own filters, sort, and
/// limit, and fetches only the sources that failed. Clones share one cache, so it
/// can outlive a `Search` rebuilt from new config.
#[derive(Clone, Default)]
pub struct ResultCache {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    answers: HashMap<AnswerKey, (Instant, Arc<Vec<Observation>>)>,
    recent: HashMap<ReleaseId, Recent>,
    seq: u64,
}

struct Recent {
    seq: u64,
    release: Release,
    saved: Option<SavedLink>,
}

impl State {
    fn trim(&mut self) {
        if self.recent.len() > MAX_RECENT {
            let mut seqs: Vec<u64> = self.recent.values().map(|r| r.seq).collect();
            seqs.sort_unstable();
            let cutoff = seqs[seqs.len() - MAX_RECENT];
            self.recent.retain(|_, r| r.seq >= cutoff);
        }
    }

    fn insert(&mut self, release: Release, saved: Option<SavedLink>) {
        self.seq += 1;
        self.recent.insert(release.id, Recent { seq: self.seq, release, saved });
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct AnswerKey {
    pub source: String,
    pub scope: String,
    pub text: String,
    pub adult: bool,
    /// A title's search, whose answers also list the items' files.
    pub title: bool,
}

impl ResultCache {
    pub fn new() -> ResultCache {
        ResultCache::default()
    }

    /// Releases from recent searches whose ID starts with `prefix`, newest first.
    /// More than one match means the prefix is ambiguous.
    ///
    /// Unlike a `SearchResult`, these keep links from private sources, which can
    /// carry a Torznab API key or a tracker passkey: they are for downloading,
    /// not for printing.
    pub fn find(&self, prefix: &HexPrefix) -> Vec<Release> {
        let state = self.lock();
        let mut found: Vec<&Recent> = state.recent.values().filter(|r| prefix.matches(&r.release.id)).collect();
        found.sort_by_key(|r| std::cmp::Reverse(r.seq));
        found.into_iter().map(|r| r.release.clone()).collect()
    }

    /// The private `.torrent` link of a recent release, without its API key, for an engine to
    /// save. `None` when its link is public, or it has none.
    pub fn saved_link(&self, id: &ReleaseId) -> Option<SavedLink> {
        self.lock().recent.get(id)?.saved.clone()
    }

    pub(crate) fn answer(&self, key: &AnswerKey, now: Instant) -> Option<Arc<Vec<Observation>>> {
        let state = self.lock();
        let (at, rows) = state.answers.get(key)?;
        (now.saturating_duration_since(*at) < TTL).then(|| rows.clone())
    }

    pub(crate) fn store(&self, key: AnswerKey, rows: Arc<Vec<Observation>>, now: Instant) {
        let mut state = self.lock();
        if state.answers.len() >= MAX_ANSWERS {
            state.answers.retain(|_, (at, _)| now.saturating_duration_since(*at) < TTL);
        }
        if state.answers.len() >= MAX_ANSWERS
            && let Some(oldest) = state.answers.iter().min_by_key(|(_, (at, _))| *at).map(|(k, _)| k.clone())
        {
            state.answers.remove(&oldest);
        }
        state.answers.insert(key, (now, rows));
    }

    /// Remembers what an earlier engine process saved, newest first, so their IDs
    /// still resolve. Call it before the first search: it never replaces a release
    /// this cache already holds. Shown releases carry no private links, and saved
    /// links carry no key.
    pub fn restore(&self, saved: Vec<(Release, Option<SavedLink>)>) {
        let mut state = self.lock();
        for (release, link) in saved.into_iter().rev() {
            if !state.recent.contains_key(&release.id) {
                state.insert(release, link);
            }
        }
        state.trim();
    }

    pub(crate) fn remember(&self, releases: Vec<(Release, Option<SavedLink>)>) {
        let mut state = self.lock();
        for (release, saved) in releases {
            state.insert(release, saved);
        }
        state.trim();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::Identity;

    fn rows(name: &str) -> Arc<Vec<Observation>> {
        let hash = crate::observation::infohash(&"ab".repeat(20)).unwrap();
        Arc::new(vec![Observation::new(Identity::Hash { hash, torrent_url: None }, name).unwrap()])
    }

    #[test]
    fn answers_expire_after_five_minutes() {
        let cache = ResultCache::new();
        let key = AnswerKey {
            source: "knaben".into(),
            scope: "https://api.knaben.org/".into(),
            text: "ubuntu".into(),
            adult: false,
            title: false,
        };
        let t0 = Instant::now();
        cache.store(key.clone(), rows("Ubuntu 24.04"), t0);
        let hit = cache.answer(&key, t0 + Duration::from_secs(299)).map(|r| r[0].name().to_string());
        assert_eq!(hit.as_deref(), Some("Ubuntu 24.04"));
        assert!(cache.answer(&key, t0 + Duration::from_secs(301)).is_none());
        let adult = AnswerKey { adult: true, ..key };
        assert!(cache.answer(&adult, t0).is_none());
    }
}
