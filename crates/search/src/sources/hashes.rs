use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use torrential_core::InfoHash;

use crate::metainfo::Metainfo;
use crate::store::Store;

const FILE: &str = "torrent-hashes.json";
const KEEP: usize = 256;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Key {
    pub url: String,
    pub modified: Option<i64>,
}

pub(crate) struct Hashes {
    store: Option<Arc<Store>>,
    known: Mutex<HashMap<Key, Metainfo>>,
    claimed: Mutex<HashSet<Key>>,
    unsaved: AtomicBool,
    saving: tokio::sync::Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct Stored {
    url: String,
    modified: Option<i64>,
    hash: InfoHash,
    size: Option<u64>,
    trackers: Vec<String>,
}

impl Hashes {
    /// Reads what an earlier process kept in `dir`, which `store` writes to.
    pub fn new(dir: Option<PathBuf>, store: Option<Arc<Store>>) -> Hashes {
        let known = dir.map(|d| parse(&std::fs::read(d.join(FILE)).unwrap_or_default())).unwrap_or_default();
        Hashes {
            store,
            known: Mutex::new(known),
            claimed: Mutex::default(),
            unsaved: AtomicBool::new(false),
            saving: tokio::sync::Mutex::new(()),
        }
    }

    pub fn claim(&self, key: &Key) -> bool {
        !self.lock().contains_key(key) && self.claimed.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone())
    }

    pub fn get(&self, key: &Key) -> Option<Metainfo> {
        self.lock().get(key).cloned()
    }

    pub fn insert(&self, key: Key, meta: Metainfo) {
        let mut known = self.lock();
        known.insert(key, meta);
        trim(&mut known);
        self.unsaved.store(true, Ordering::Relaxed);
    }

    pub async fn save(&self) {
        let Some(store) = &self.store else { return };
        let _one = self.saving.lock().await;
        if !self.unsaved.swap(false, Ordering::Relaxed) {
            return;
        }
        let mut all = store.get(FILE).await.map(|kept| parse(&kept.bytes)).unwrap_or_default();
        all.extend(self.lock().iter().map(|(k, v)| (k.clone(), v.clone())));
        trim(&mut all);
        let mut stored: Vec<Stored> = all
            .into_iter()
            .map(|(Key { url, modified }, Metainfo { hash, size, trackers })| Stored {
                url,
                modified,
                hash,
                size,
                trackers,
            })
            .collect();
        stored.sort_by(|a, b| (&a.url, a.modified).cmp(&(&b.url, b.modified)));
        let body = serde_json::to_vec_pretty(&stored).expect("stored hashes serialize");
        if !store.put(FILE, &body).await {
            self.unsaved.store(true, Ordering::Relaxed);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Metainfo>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn parse(body: &[u8]) -> HashMap<Key, Metainfo> {
    let stored: Vec<Stored> = serde_json::from_slice(body).unwrap_or_default();
    stored
        .into_iter()
        .map(|s| {
            (Key { url: s.url, modified: s.modified }, Metainfo { hash: s.hash, size: s.size, trackers: s.trackers })
        })
        .collect()
}

fn trim(known: &mut HashMap<Key, Metainfo>) {
    if known.len() > KEEP {
        let mut keys: Vec<Key> = known.keys().cloned().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.modified));
        for key in &keys[KEEP..] {
            known.remove(key);
        }
    }
}
