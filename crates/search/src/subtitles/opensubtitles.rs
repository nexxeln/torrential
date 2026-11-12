//! OpenSubtitles, through the public Stremio add-on that lists its subtitles by IMDb ID. It
//! needs no key.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use tokio::time::Instant;
use url::Url;

use super::{BoxFuture, Provider, Subtitle, Wanted};
use crate::http::{FetchError, Http};

pub const NAME: &str = "opensubtitles";
pub const BASE: &str = "https://opensubtitles-v3.strem.io/";
const LARGEST: usize = 2 << 20;
/// How the add-on marks a subtitle it matched by the file's hash. The others it matched by IMDb ID (`i`).
const BY_HASH: &str = "h";

pub struct OpenSubtitles {
    pub base: Url,
}

#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    subtitles: Vec<Listed>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    id: String,
    url: String,
    lang: String,
    /// How it matched.
    m: Option<String>,
    subtitle_file_name: Option<String>,
    movie_release_name: Option<String>,
}

impl OpenSubtitles {
    /// `subtitles/movie/tt1727587.json` for a film and `subtitles/series/tt0903747:3:5.json` for an
    /// episode, with the file's hash, size, and name as one more path segment when the hash is known.
    fn url(&self, wanted: &Wanted) -> Result<Url, url::ParseError> {
        let mut path = match wanted.episode {
            Some(e) => format!("subtitles/series/{}:{}:{}", wanted.imdb, e.season, e.episode),
            None => format!("subtitles/movie/{}", wanted.imdb),
        };
        if let Some(file) = wanted.hash {
            let name = utf8_percent_encode(wanted.file_name, NON_ALPHANUMERIC);
            path.push_str(&format!("/videoHash={:016x}&videoSize={}&filename={name}", file.hash, file.size));
        }
        self.base.join(&format!("{path}.json"))
    }
}

impl Provider for OpenSubtitles {
    fn search<'a>(
        &'a self,
        http: &'a Http,
        wanted: &'a Wanted<'a>,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<Vec<Subtitle>, FetchError>> {
        Box::pin(async move {
            let url = self.url(wanted).map_err(|e| FetchError::Connect(e.to_string()))?;
            let body = http.get(&url, LARGEST, deadline).await?;
            let listing: Listing = serde_json::from_slice(&body).map_err(|e| FetchError::Decode(e.to_string()))?;
            let named = |name: Option<String>| name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
            let subtitles = listing.subtitles.into_iter().map(|listed| Subtitle {
                id: listed.id,
                language: listed.lang,
                name: named(listed.movie_release_name).or(named(listed.subtitle_file_name)),
                exact: listed.m.as_deref() == Some(BY_HASH),
                source: NAME,
                url: listed.url,
            });
            Ok(subtitles.collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::FileHash;
    use super::*;
    use torrential_core::title::Episode;

    #[test]
    fn a_search_names_the_title_and_the_file_when_its_hash_is_known() {
        let provider = OpenSubtitles { base: Url::parse(BASE).unwrap() };
        let url = |episode, hash| {
            let wanted = Wanted { imdb: "tt1727587", episode, hash, file_name: "Sintel (2010) [1080p].mkv" };
            provider.url(&wanted).unwrap().to_string()
        };
        assert_eq!(url(None, None), "https://opensubtitles-v3.strem.io/subtitles/movie/tt1727587.json");
        assert_eq!(
            url(Some(Episode { season: 3, episode: 5 }), None),
            "https://opensubtitles-v3.strem.io/subtitles/series/tt1727587:3:5.json"
        );
        assert_eq!(
            url(None, Some(FileHash { hash: 0x8e24_5d96_79d3_1e12, size: 12_909_756 })),
            "https://opensubtitles-v3.strem.io/subtitles/movie/tt1727587/\
             videoHash=8e245d9679d31e12&videoSize=12909756&filename=Sintel%20%282010%29%20%5B1080p%5D%2Emkv.json"
        );
    }
}
