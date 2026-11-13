//! Where the catalog keeps what it fetched: files in a folder, or a map in memory. Each item has
//! a name such as `titles/tt0063350.json` and the time it was last written or used.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The folder of each title's details, kept as Cinemeta answered them.
pub const TITLES: &str = "titles";
/// The folder of pictures, and of the misses remembered for a day.
pub const PICTURES: &str = "pictures";

pub enum Shelf {
    Disk(PathBuf),
    Memory(Mutex<HashMap<String, Kept>>),
}

#[derive(Clone)]
pub struct Kept {
    pub bytes: Arc<Vec<u8>>,
    /// When it was written, or for a picture when it was last served.
    pub at: SystemTime,
}

const TMP: &str = ".tmp";

impl Shelf {
    pub fn new(dir: Option<PathBuf>) -> Shelf {
        dir.map_or_else(|| Shelf::Memory(Mutex::default()), Shelf::Disk)
    }

    pub async fn get(&self, name: &str) -> Option<Kept> {
        match self {
            Shelf::Disk(dir) => {
                let path = dir.join(name);
                let at = tokio::fs::metadata(&path).await.ok()?.modified().ok()?;
                Some(Kept { bytes: Arc::new(tokio::fs::read(&path).await.ok()?), at })
            }
            Shelf::Memory(items) => lock(items).get(name).cloned(),
        }
    }

    /// Writes all of `bytes` or nothing, so a reader never sees half an item.
    pub async fn put(&self, name: &str, bytes: &[u8]) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        match self {
            Shelf::Disk(dir) => {
                let path = dir.join(name);
                let Some(folder) = path.parent() else { return };
                let n = NEXT.fetch_add(1, Ordering::Relaxed);
                let tmp = dir.join(format!("{name}.{}.{n}{TMP}", std::process::id()));
                let written = async {
                    tokio::fs::create_dir_all(folder).await?;
                    tokio::fs::write(&tmp, bytes).await?;
                    tokio::fs::rename(&tmp, &path).await
                };
                if written.await.is_err() {
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
            }
            Shelf::Memory(items) => {
                lock(items).insert(name.to_string(), Kept { bytes: Arc::new(bytes.to_vec()), at: SystemTime::now() });
            }
        }
    }

    /// Marks `name` used now, so trimming keeps it longer.
    pub async fn touch(&self, name: &str) {
        match self {
            Shelf::Disk(dir) => {
                let path = dir.join(name);
                let _ = tokio::task::spawn_blocking(move || {
                    std::fs::File::options().write(true).open(path)?.set_modified(SystemTime::now())
                })
                .await;
            }
            Shelf::Memory(items) => {
                if let Some(kept) = lock(items).get_mut(name) {
                    kept.at = SystemTime::now();
                }
            }
        }
    }

    /// Removes the least recently used items in `folder` until the rest take at most `budget` bytes.
    pub async fn trim(&self, folder: &str, budget: u64) {
        match self {
            Shelf::Disk(dir) => {
                let dir = dir.join(folder);
                let _ = tokio::task::spawn_blocking(move || {
                    let mut items = Vec::new();
                    for entry in std::fs::read_dir(&dir)? {
                        let entry = entry?;
                        let meta = entry.metadata()?;
                        if meta.is_file() && !entry.file_name().to_string_lossy().ends_with(TMP) {
                            items.push((meta.modified()?, meta.len(), entry.path()));
                        }
                    }
                    for path in oldest_over(items, budget) {
                        let _ = std::fs::remove_file(path);
                    }
                    std::io::Result::Ok(())
                })
                .await;
            }
            Shelf::Memory(items) => {
                let mut items = lock(items);
                let prefix = format!("{folder}/");
                let listed = items.iter().filter(|(name, _)| name.starts_with(&prefix));
                let listed = listed.map(|(name, kept)| (kept.at, kept.bytes.len() as u64, name.clone())).collect();
                for name in oldest_over(listed, budget) {
                    items.remove(&name);
                }
            }
        }
    }
}

/// The items to remove, oldest first, so the rest take at most `budget` bytes.
fn oldest_over<T>(mut items: Vec<(SystemTime, u64, T)>, budget: u64) -> Vec<T> {
    let mut total: u64 = items.iter().map(|(_, size, _)| size).sum();
    items.sort_by_key(|(at, _, _)| *at);
    let mut removed = Vec::new();
    for (_, size, item) in items {
        if total <= budget {
            break;
        }
        total -= size;
        removed.push(item);
    }
    removed
}

fn lock(items: &Mutex<HashMap<String, Kept>>) -> std::sync::MutexGuard<'_, HashMap<String, Kept>> {
    items.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    async fn trimmed(shelf: &Shelf) -> Vec<(String, bool)> {
        let names = ["pictures/old", "pictures/used", "pictures/new", "titles/old"];
        for name in names {
            shelf.put(name, &[0; 10]).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        shelf.touch("pictures/used").await;
        shelf.trim("pictures", 10).await;
        let mut kept = Vec::new();
        for name in names {
            kept.push((name.to_string(), shelf.get(name).await.is_some()));
        }
        kept
    }

    #[tokio::test]
    async fn trimming_drops_the_least_recently_used_until_under_budget() {
        let expected =
            [("pictures/old", false), ("pictures/used", true), ("pictures/new", false), ("titles/old", true)];
        let expected: Vec<(String, bool)> = expected.map(|(name, kept)| (name.to_string(), kept)).into();
        let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("torrential-shelf-{}-{nanos}", std::process::id()));
        assert_eq!(trimmed(&Shelf::new(Some(dir.clone()))).await, expected, "on disk");
        assert_eq!(trimmed(&Shelf::new(None)).await, expected, "in memory");
        let _ = std::fs::remove_dir_all(dir);
    }
}
