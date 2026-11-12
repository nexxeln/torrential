//! IMDb's suggestions: the titles its own search box lists for some words, best known first. They
//! need no key. The catalog searches with them, and subtitles find a file's IMDb ID with them.

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use tokio::time::Instant;
use torrential_core::{Kind, TitleCard};
use url::Url;

use super::Listed;
use crate::http::{FetchError, Http};

pub const BASE: &str = "https://v3.sg.media-imdb.com/";
const LARGEST: usize = 256 << 10;

/// The kinds of title the catalog lists, by IMDb's name. Anything else, such as a video, a game,
/// a single episode, or a person, it leaves out.
const KINDS: [(&str, Kind); 6] = [
    ("movie", Kind::Film),
    ("short", Kind::Film),
    ("tvMovie", Kind::Film),
    ("tvSpecial", Kind::Film),
    ("tvSeries", Kind::Series),
    ("tvMiniSeries", Kind::Series),
];

/// Wide enough for a card and a title page. IMDb's original poster is often 1000 or more.
const POSTER_WIDTH: u32 = 300;

#[derive(Deserialize)]
struct Suggestions {
    #[serde(default)]
    d: Vec<Suggestion>,
}

#[derive(Deserialize)]
pub struct Suggestion {
    /// A title's ID such as `tt1727587`, or a person's such as `nm0000001`.
    pub id: String,
    /// The title. People are listed too, by name.
    #[serde(default)]
    pub l: String,
    pub y: Option<u16>,
    /// The kind of title, such as `movie` or `tvSeries`. People have none.
    pub qid: Option<String>,
    /// A title's poster, or a person's photo.
    pub i: Option<Image>,
}

#[derive(Deserialize)]
pub struct Image {
    #[serde(rename = "imageUrl")]
    pub url: String,
}

/// What IMDb suggests for `words`, in its order.
pub async fn suggestions(
    http: &Http,
    base: &Url,
    words: &str,
    deadline: Instant,
) -> Result<Vec<Suggestion>, FetchError> {
    let path = format!("suggestion/x/{}.json", utf8_percent_encode(words, NON_ALPHANUMERIC));
    let url = base.join(&path).map_err(|e| FetchError::Connect(e.to_string()))?;
    let body = http.get(&url, LARGEST, deadline).await?;
    let suggestions: Suggestions = serde_json::from_slice(&body).map_err(|e| FetchError::Decode(e.to_string()))?;
    Ok(suggestions.d)
}

/// The films and series among `suggestions`, in IMDb's order, each with its poster about 300
/// pixels wide.
pub(super) fn listed(suggestions: Vec<Suggestion>) -> Vec<Listed> {
    let listed = suggestions.into_iter().filter_map(|suggestion| {
        let (_, kind) = KINDS.iter().find(|(name, _)| suggestion.qid.as_deref() == Some(*name))?;
        let name = suggestion.l.trim();
        let card = TitleCard {
            id: suggestion.id.parse().ok()?,
            kind: *kind,
            name: (!name.is_empty()).then(|| name.to_string())?,
            year: suggestion.y,
            rating: None,
        };
        let poster = suggestion.i.and_then(|image| Url::parse(image.url.trim()).ok()).map(sized);
        Some(Listed { card, poster })
    });
    listed.collect()
}

/// `poster` about 300 pixels wide. IMDb's pictures are Amazon's, which resize to what a `._V1_SX300`
/// before the extension says, in place of whatever cropping or sizing the address had.
fn sized(mut poster: Url) -> Url {
    let path = poster.path();
    if let Some((stem, extension)) = path.rsplit_once('.').filter(|(_, extension)| !extension.contains('/')) {
        let stem = stem.find("._V1").map_or(stem, |at| &stem[..at]);
        let sized = format!("{stem}._V1_SX{POSTER_WIDTH}.{extension}");
        poster.set_path(&sized);
    }
    poster
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poster_is_asked_for_300_pixels_wide() {
        let amazon = "https://m.media-amazon.com/images/M/";
        let cases = [
            ("MV5BMzc5NTUz@@._V1_.jpg", "MV5BMzc5NTUz@@._V1_SX300.jpg"),
            ("MV5BMDc1MDE0@._V1_CR276,54,1648,2473_.jpg", "MV5BMDc1MDE0@._V1_SX300.jpg"),
            ("MV5BMDc1MDE0@._V1_UX182_CR0,0,182,268_AL_.png", "MV5BMDc1MDE0@._V1_SX300.png"),
            ("MV5BNDA3OGZk.jpg", "MV5BNDA3OGZk._V1_SX300.jpg"),
        ];
        for (original, expected) in cases {
            let poster = sized(Url::parse(&format!("{amazon}{original}")).unwrap());
            assert_eq!(poster.as_str(), format!("{amazon}{expected}"));
        }
        let plain = "https://example.com/v1.2/poster";
        assert_eq!(sized(Url::parse(plain).unwrap()).as_str(), plain, "an address without an extension stays");
    }
}
