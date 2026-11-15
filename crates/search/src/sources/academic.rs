//! Academic Torrents publishes its whole catalog as one RSS file and has no search
//! endpoint, so the catalog is kept on disk for a day and searched locally.

use std::sync::Arc;
use std::time::SystemTime;

use torrential_core::Category;
use url::Url;

use super::{Ctx, Failure, at, rss};
use crate::observation::{Identity, Observation, infohash, web_url};
use crate::store::{FRESH_FOR, Store};

const CAP: usize = 32 << 20;
const FILE: &str = "academictorrents.xml";
const MAX_RESULTS: usize = 100;

pub(crate) struct Catalog {
    store: Option<Arc<Store>>,
    loaded: tokio::sync::Mutex<Option<Loaded>>,
}

struct Loaded {
    fetched: SystemTime,
    items: Arc<Vec<Observation>>,
}

pub(super) async fn search(ctx: &Ctx<'_>, base: &Url) -> Result<Vec<Observation>, Failure> {
    let items = ctx.stores.academic.items(ctx, base).await?;
    Ok(items.iter().filter(|o| ctx.terms.covers(o.name())).take(MAX_RESULTS).cloned().collect())
}

impl Catalog {
    pub fn new(store: Option<Arc<Store>>) -> Catalog {
        Catalog { store, loaded: tokio::sync::Mutex::new(None) }
    }

    async fn items(&self, ctx: &Ctx<'_>, base: &Url) -> Result<Arc<Vec<Observation>>, Failure> {
        let mut loaded = self.loaded.lock().await;
        if loaded.is_none() {
            *loaded = self.read_disk(base).await;
        }
        let now = SystemTime::now();
        if let Some(l) = &*loaded
            && now.duration_since(l.fetched).is_ok_and(|age| age < FRESH_FOR)
        {
            return Ok(l.items.clone());
        }
        let fetched = match ctx.http.get(&at(base, "database.xml"), CAP, ctx.deadline).await {
            Ok(body) => parse(&body, base).map(|items| (body, items)).map_err(Failure::BadResponse),
            Err(e) => Err(e.into()),
        };
        match fetched {
            Ok((body, items)) => {
                self.write_disk(&body).await;
                let items = Arc::new(items);
                *loaded = Some(Loaded { fetched: now, items: items.clone() });
                Ok(items)
            }
            Err(failure) => {
                let stale = loaded.as_ref().map(|l| l.items.clone()).ok_or(failure)?;
                ctx.mark_partial();
                Ok(stale)
            }
        }
    }

    async fn read_disk(&self, base: &Url) -> Option<Loaded> {
        let kept = self.store.as_ref()?.get(FILE).await?;
        Some(Loaded { fetched: kept.at, items: Arc::new(parse(&kept.bytes, base).ok()?) })
    }

    async fn write_disk(&self, body: &[u8]) {
        if let Some(store) = &self.store {
            store.put(FILE, body).await;
        }
    }
}

fn parse(body: &[u8], base: &Url) -> Result<Vec<Observation>, String> {
    Ok(rss::items(body)?
        .into_iter()
        .filter_map(|item| {
            let hash = infohash(item.get("infohash")?)?;
            let torrent_url = web_url(at(base, &format!("download/{}.torrent", hash.id().to_hex())).as_str());
            let mut obs = Observation::new(Identity::Hash { hash, torrent_url }, item.get("title")?)?;
            obs.size = item.get("size").and_then(|s| s.trim().parse().ok());
            obs.category = item.get("category").map(|c| match c.trim() {
                "Dataset" => Category::Dataset,
                "Paper" => Category::Book,
                "Course" => Category::Video,
                _ => Category::Other,
            });
            obs.info_url = item.get("link").and_then(web_url);
            Some(obs)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::fixture;

    #[test]
    fn reads_catalog_entries() {
        let base = Url::parse("https://academictorrents.com/").unwrap();
        let all = parse(&fixture::read("academictorrents/database.xml"), &base).unwrap();
        assert_eq!(all.len(), 40);
        let first = &all[0];
        assert_eq!(first.name(), "Stack Exchange Data Dump (2026-09-30)");
        assert_eq!(
            first.identity,
            Identity::Hash {
                hash: infohash("c32cdd0306ee7717cd63f98f8df6486af55f79c8").unwrap(),
                torrent_url: Some(
                    "https://academictorrents.com/download/c32cdd0306ee7717cd63f98f8df6486af55f79c8.torrent".into()
                ),
            }
        );
        assert_eq!(first.size, Some(99_497_279_488));
        assert_eq!(first.category, Some(Category::Dataset));
        assert_eq!(first.peers, None);
        assert_eq!(
            first.info_url.as_deref(),
            Some("https://academictorrents.com/details/c32cdd0306ee7717cd63f98f8df6486af55f79c8")
        );
    }
}
