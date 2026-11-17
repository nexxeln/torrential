//! Web seeds to add to a torrent's own, so that every file has a source that serves all of it.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::future::Either;
use torrential_core::Error;

use crate::{Inner, ResultCache, Search, SearchConfig, redirect};

/// Looks up web seeds for a torrent being downloaded, with the sources and clients of the search
/// it came from, so a disabled archive.org source or a replaced endpoint applies here too.
#[derive(Clone)]
pub struct WebSeeds(pub(crate) Arc<Inner>);

impl WebSeeds {
    /// The lookup of a search of its own, for a process that downloads without searching.
    pub fn new(config: &SearchConfig) -> Result<WebSeeds, Error> {
        Ok(Search::new(config, ResultCache::new())?.web_seeds())
    }

    /// Web seeds that serve every file of a torrent, to add to its own. A web seed can answer each
    /// file with a redirect to a server that holds it, as archive.org's download URL does, and
    /// libtorrent turns each redirect into a web seed that has only that file, so a piece that
    /// spans two files has no source. archive.org names the servers of a torrent that downloads
    /// from it, unless the archive.org source is disabled. For any other torrent, the seeds are the
    /// folders its web seeds redirect `file` to, where `file` is one of its several files as a path
    /// that starts with the torrent's name. `None` when there is nothing to ask.
    pub fn lookup(
        &self,
        name: &str,
        file: Option<&Path>,
        web_seeds: &[String],
    ) -> Option<impl Future<Output = Result<Vec<String>, Error>> + Send + 'static + use<>> {
        let inner = self.0.clone();
        if let Some(item) = self.0.registry().enabled.iter().find_map(|s| s.archive_item(name, web_seeds)) {
            let lookup = match self.0.asked_servers(&item) {
                Some(asked) => asked.boxed(),
                None => async move { item.mirrors(&inner.http).await }.boxed(),
            };
            return Some(Either::Left(lookup));
        }
        let file = file.filter(|file| web_seeds.iter().any(|seed| redirect::request(seed, file).is_some()))?;
        let (file, web_seeds) = (file.to_path_buf(), web_seeds.to_vec());
        Some(Either::Right(async move { Ok(redirect::roots(&inner.seeds, &file, &web_seeds).await) }))
    }
}
