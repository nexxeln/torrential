use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sha1::{Digest, Sha1};
use tokio::time::Instant;
use url::Url;

use super::{Ctx, Failure};
use crate::http::Http;
use crate::observation::hex;
use crate::store::{self, FRESH_FOR, Store};

const REFETCH_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const REFRESH_WITHIN: Duration = Duration::from_secs(30);

pub(super) type Check = fn(&[u8]) -> bool;

#[derive(Clone)]
pub(crate) struct Pages {
    store: Option<Arc<Store>>,
    refreshing: Arc<Mutex<HashSet<Url>>>,
}

impl Pages {
    pub fn new(store: Option<Arc<Store>>) -> Pages {
        Pages { store, refreshing: Arc::default() }
    }

    pub async fn get(&self, ctx: &Ctx<'_>, url: &Url, cap: usize, check: Check) -> Result<Vec<u8>, Failure> {
        let kept = if ctx.fresh { None } else { self.read(url, check).await };
        match kept {
            Some((age, body)) if age < FRESH_FOR => return Ok(body),
            Some((age, body)) if age < REFETCH_AFTER => {
                self.refresh(ctx.http, url, cap, check);
                ctx.mark_partial();
                return Ok(body);
            }
            _ => {}
        }
        match (ctx.http.get(url, cap, ctx.deadline).await, kept) {
            (Ok(body), _) if check(&body) => {
                self.write(url, &body).await;
                Ok(body)
            }
            (_, Some((_, old))) => {
                ctx.mark_partial();
                Ok(old)
            }
            (fetched, None) => Ok(fetched?),
        }
    }

    fn refresh(&self, http: &Http, url: &Url, cap: usize, check: Check) {
        if !self.lock().insert(url.clone()) {
            return;
        }
        let (pages, http, url) = (self.clone(), http.detached(), url.clone());
        tokio::spawn(async move {
            if let Ok(body) = http.get(&url, cap, Instant::now() + REFRESH_WITHIN).await
                && check(&body)
            {
                pages.write(&url, &body).await;
            }
            pages.lock().remove(&url);
        });
    }

    async fn read(&self, url: &Url, check: Check) -> Option<(Duration, Vec<u8>)> {
        let kept = self.store.as_ref()?.get(&file_name(url)).await?;
        check(&kept.bytes).then(|| (kept.age(), kept.bytes.to_vec()))
    }

    async fn write(&self, url: &Url, body: &[u8]) {
        if let Some(store) = &self.store {
            store.put(&file_name(url), body).await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<Url>> {
        self.refreshing.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn file_name(url: &Url) -> String {
    format!("{}/{}", store::PAGES, hex(&Sha1::digest(url.as_str().as_bytes())))
}
