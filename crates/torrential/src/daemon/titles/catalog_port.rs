//! Films and series: their details, pictures, and what to search for to find one.

use std::sync::Arc;
use std::time::Duration;

use torrential_core::title::{Episode, Namesakes, Wanted, normalized};
use torrential_core::{
    Art, ArtKind, EpisodeInfo, Error, ErrorCode, Kind, Season, TitleCard, TitleDetails, TitleId, VideoRef,
};
use torrential_search::catalog::{Catalog, CatalogConfig, ImageFormat, Picture};

use super::super::search_port::BoxFuture;

pub trait CatalogPort: Send + Sync {
    /// Films and series named like `words`, exact names first. Empty when lookups are off.
    fn search<'a>(&'a self, words: &'a str) -> BoxFuture<'a, Result<Vec<TitleCard>, Error>>;
    fn title<'a>(&'a self, id: &'a TitleId) -> BoxFuture<'a, Result<TitleDetails, Error>>;
    /// The titles of `card`'s kind under its normalized name, `card` among them, in the order the
    /// catalog lists them.
    fn namesakes<'a>(&'a self, card: &'a TitleCard) -> BoxFuture<'a, Result<Vec<TitleCard>, Error>>;
    fn art<'a>(&'a self, id: &'a TitleId, kind: ArtKind) -> BoxFuture<'a, Result<Picture, Error>>;

    /// The details of `video`'s title, and how to search for the film or the episode.
    fn wanted<'a>(&'a self, video: &'a VideoRef) -> BoxFuture<'a, Result<(TitleDetails, Wanted), Error>> {
        Box::pin(async move {
            let details = self.title(&video.title).await?;
            let namesakes = self.namesakes(&details.card).await?;
            let wanted = wanted(&details, &namesakes, video.episode)?;
            Ok((details, wanted))
        })
    }
}

/// How to tell the releases of `episode` of the title `details` describes, or of the film, from
/// others. `namesakes` are the titles of its kind that share its name, in the catalog's order.
pub fn wanted(details: &TitleDetails, namesakes: &[TitleCard], episode: Option<Episode>) -> Result<Wanted, Error> {
    let card = &details.card;
    let season_episodes = match (card.kind, episode) {
        (Kind::Film, None) => None,
        (Kind::Film, Some(e)) => {
            return Err(Error::new(ErrorCode::NotFound, format!("{} is a film, so it has no {}", card.name, label(e)))
                .with_hint(format!("play {} without an episode", card.id)));
        }
        (Kind::Series, None) => {
            return Err(Error::new(
                ErrorCode::Usage,
                format!("{} is a series, so name one of its episodes", card.name),
            )
            .with_hint(format!("name an episode such as {}:1:1", card.id)));
        }
        (Kind::Series, Some(e)) => {
            let season = details.seasons.iter().find(|s| s.number == e.season);
            if !season.is_some_and(|s| s.episodes.iter().any(|info| info.episode == e)) {
                return Err(Error::new(
                    ErrorCode::NotFound,
                    format!("{} is not an episode of {}", label(e), card.name),
                ));
            }
            season.map(|s| s.episodes.len() as u32)
        }
    };
    let shared = namesakes.iter().any(|other| other.id != card.id);
    let namesakes = match namesakes.first() {
        _ if !shared => Namesakes::None,
        Some(first) if first.id == card.id => Namesakes::Leading,
        _ => Namesakes::Shadowed,
    };
    Ok(Wanted {
        name: card.name.clone(),
        year: card.year,
        episode,
        namesakes,
        runtime: details.runtime_min.map(|minutes| Duration::from_secs(u64::from(minutes) * 60)),
        season_episodes,
    })
}

/// An episode as people write it, such as S03E05.
pub fn label(episode: Episode) -> String {
    format!("S{:02}E{:02}", episode.season, episode.episode)
}

pub struct Live(Catalog);

impl Live {
    pub fn new(config: &CatalogConfig) -> Result<Self, Error> {
        Catalog::new(config).map(Self)
    }
}

impl CatalogPort for Live {
    fn search<'a>(&'a self, words: &'a str) -> BoxFuture<'a, Result<Vec<TitleCard>, Error>> {
        Box::pin(self.0.search(words))
    }

    fn title<'a>(&'a self, id: &'a TitleId) -> BoxFuture<'a, Result<TitleDetails, Error>> {
        Box::pin(self.0.title(id))
    }

    fn namesakes<'a>(&'a self, card: &'a TitleCard) -> BoxFuture<'a, Result<Vec<TitleCard>, Error>> {
        Box::pin(self.0.namesakes(card))
    }

    fn art<'a>(&'a self, id: &'a TitleId, kind: ArtKind) -> BoxFuture<'a, Result<Picture, Error>> {
        Box::pin(self.0.art(id, kind))
    }
}

/// A 1 by 1 pixel PNG picture.
const PIXEL: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

/// Answers without the network: Night of the Living Dead (tt0063350), its 1990 remake
/// (tt0100258), and Harbor Lights (tt9999901), a series of two seasons with a special and an
/// episode that has not aired. Every picture it has is one pixel.
pub struct Fake {
    titles: Vec<TitleDetails>,
}

impl Default for Fake {
    fn default() -> Self {
        Self { titles: vec![night(), remake(), harbor_lights()] }
    }
}

fn card(id: &str, kind: Kind, name: &str, year: u16, rating: f32) -> TitleCard {
    TitleCard {
        id: id.parse().expect("fake title IDs are valid"),
        kind,
        name: name.into(),
        year: Some(year),
        rating: Some(rating),
    }
}

fn night() -> TitleDetails {
    TitleDetails {
        card: card("tt0063350", Kind::Film, "Night of the Living Dead", 1968, 7.8),
        synopsis: Some("Strangers shelter in a farmhouse while the dead walk outside.".into()),
        genres: vec!["Horror".into(), "Thriller".into()],
        runtime_min: Some(96),
        cast: vec!["Duane Jones".into(), "Judith O'Dea".into(), "Karl Hardman".into()],
        art: Art { poster: true, background: true, logo: true },
        seasons: Vec::new(),
    }
}

fn remake() -> TitleDetails {
    TitleDetails {
        card: card("tt0100258", Kind::Film, "Night of the Living Dead", 1990, 6.9),
        synopsis: None,
        genres: vec!["Horror".into()],
        runtime_min: Some(88),
        cast: vec!["Tony Todd".into(), "Patricia Tallman".into()],
        art: Art::default(),
        seasons: Vec::new(),
    }
}

/// Never, as far as a test runs.
const UNAIRED: i64 = 4_102_444_800;

fn harbor_lights() -> TitleDetails {
    let episode = |season, episode, name: &str, aired, thumbnail| EpisodeInfo {
        episode: Episode { season, episode },
        name: Some(name.into()),
        aired,
        overview: None,
        thumbnail,
    };
    let seasons = vec![
        Season { number: 0, episodes: vec![episode(0, 1, "Lights Out", Some(1_608_768_000), false)] },
        Season {
            number: 1,
            episodes: vec![
                episode(1, 1, "Pilot", Some(1_556_668_800), true),
                episode(1, 2, "Undertow", Some(1_557_273_600), true),
                episode(1, 3, "Fog Bell", Some(1_557_878_400), false),
            ],
        },
        Season {
            number: 2,
            episodes: vec![
                episode(2, 1, "Return", Some(1_588_291_200), false),
                episode(2, 2, "Tide Tables", Some(UNAIRED), false),
                episode(2, 3, "Untitled", None, false),
            ],
        },
    ];
    TitleDetails {
        card: card("tt9999901", Kind::Series, "Harbor Lights", 2019, 8.1),
        synopsis: Some("A lighthouse keeper's family runs a harbor town's only inn.".into()),
        genres: vec!["Drama".into()],
        runtime_min: Some(44),
        cast: vec!["Ada Byrne".into()],
        art: Art { poster: true, background: false, logo: false },
        seasons,
    }
}

impl Fake {
    fn find(&self, id: &TitleId) -> Result<&TitleDetails, Error> {
        self.titles
            .iter()
            .find(|t| t.card.id == *id)
            .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("Cinemeta has no film or series {id}")))
    }
}

impl CatalogPort for Fake {
    fn search<'a>(&'a self, words: &'a str) -> BoxFuture<'a, Result<Vec<TitleCard>, Error>> {
        let words = normalized(words);
        let named = |t: &&TitleDetails| !words.is_empty() && normalized(&t.card.name).contains(&words);
        let (exact, rest): (Vec<_>, Vec<_>) =
            self.titles.iter().filter(named).partition(|t| normalized(&t.card.name) == words);
        let cards = exact.into_iter().chain(rest).map(|t| t.card.clone()).collect();
        Box::pin(async move { Ok(cards) })
    }

    fn title<'a>(&'a self, id: &'a TitleId) -> BoxFuture<'a, Result<TitleDetails, Error>> {
        let found = self.find(id).cloned();
        Box::pin(async move { found })
    }

    fn namesakes<'a>(&'a self, card: &'a TitleCard) -> BoxFuture<'a, Result<Vec<TitleCard>, Error>> {
        let name = normalized(&card.name);
        let same = self.titles.iter().filter(|t| t.card.kind == card.kind && normalized(&t.card.name) == name);
        let cards = same.map(|t| t.card.clone()).collect();
        Box::pin(async move { Ok(cards) })
    }

    fn art<'a>(&'a self, id: &'a TitleId, kind: ArtKind) -> BoxFuture<'a, Result<Picture, Error>> {
        let found = self.find(id).and_then(|t| {
            let has = match kind {
                ArtKind::Poster => t.art.poster,
                ArtKind::Background => t.art.background,
                ArtKind::Logo => t.art.logo,
                ArtKind::Thumbnail(e) => {
                    t.seasons.iter().flat_map(|s| &s.episodes).any(|info| info.episode == e && info.thumbnail)
                }
            };
            has.then(|| Picture { bytes: Arc::new(PIXEL.to_vec()), format: ImageFormat::Png })
                .ok_or_else(|| Error::new(ErrorCode::NotFound, format!("Cinemeta names no such picture for {id}")))
        });
        Box::pin(async move { found })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(text: &str) -> VideoRef {
        text.parse().unwrap()
    }

    #[test]
    fn a_title_is_wanted_by_its_name_year_and_whether_another_shares_the_name() {
        let (night, remake, series) = (night(), remake(), harbor_lights());
        let both = [night.card.clone(), remake.card.clone()];
        let read = |details: &TitleDetails, namesakes: &[TitleCard], episode| {
            wanted(details, namesakes, episode).map(|w| (w.year, w.namesakes, w.runtime, w.season_episodes))
        };
        let minutes = |m: u64| Some(Duration::from_secs(m * 60));
        assert_eq!(read(&night, &both, None), Ok((Some(1968), Namesakes::Leading, minutes(96), None)));
        assert_eq!(read(&remake, &both, None), Ok((Some(1990), Namesakes::Shadowed, minutes(88), None)));
        assert_eq!(read(&night, &both[..1], None), Ok((Some(1968), Namesakes::None, minutes(96), None)));
        assert_eq!(read(&night, &[], None), Ok((Some(1968), Namesakes::None, minutes(96), None)));
        assert_eq!(read(&remake, &both[1..], None), Ok((Some(1990), Namesakes::None, minutes(88), None)));
        let episode = |season, episode| Some(Episode { season, episode });
        assert_eq!(read(&series, &[], episode(1, 2)), Ok((Some(2019), Namesakes::None, minutes(44), Some(3))));
        assert_eq!(read(&series, &[], episode(0, 1)), Ok((Some(2019), Namesakes::None, minutes(44), Some(1))));

        let refused = |details: &TitleDetails, episode| {
            let e = wanted(details, &[], episode).unwrap_err();
            (e.code, e.message)
        };
        assert_eq!(
            refused(&night, episode(1, 1)),
            (ErrorCode::NotFound, "Night of the Living Dead is a film, so it has no S01E01".into())
        );
        assert_eq!(
            refused(&series, episode(3, 1)),
            (ErrorCode::NotFound, "S03E01 is not an episode of Harbor Lights".into())
        );
        assert_eq!(
            refused(&series, episode(1, 4)),
            (ErrorCode::NotFound, "S01E04 is not an episode of Harbor Lights".into())
        );
        assert_eq!(
            refused(&series, None),
            (ErrorCode::Usage, "Harbor Lights is a series, so name one of its episodes".into())
        );
    }

    #[tokio::test]
    async fn the_fake_catalog_finds_titles_namesakes_and_pictures_without_the_network() {
        let fake = Fake::default();
        let ids = |cards: Vec<TitleCard>| cards.into_iter().map(|c| c.id.to_string()).collect::<Vec<_>>();
        assert_eq!(ids(fake.search("night of the living dead").await.unwrap()), ["tt0063350", "tt0100258"]);
        assert_eq!(ids(fake.search("Harbor").await.unwrap()), ["tt9999901"]);
        assert_eq!(ids(fake.search("  ").await.unwrap()), Vec::<String>::new());
        let (details, remake) = fake.wanted(&video("tt0100258")).await.unwrap();
        assert_eq!((details.card.year, remake.namesakes), (Some(1990), Namesakes::Shadowed));
        let (_, episode) = fake.wanted(&video("tt9999901:2:1")).await.unwrap();
        assert_eq!((episode.namesakes, episode.season_episodes), (Namesakes::None, Some(3)));
        assert_eq!(fake.title(&"tt0000001".parse().unwrap()).await.unwrap_err().code, ErrorCode::NotFound);

        let poster = fake.art(&"tt0063350".parse().unwrap(), ArtKind::Poster).await.unwrap();
        assert_eq!(
            (poster.format, ImageFormat::of(&poster.bytes), poster.bytes.len()),
            (ImageFormat::Png, Some(ImageFormat::Png), 67)
        );
        let thumbnail = ArtKind::Thumbnail(Episode { season: 1, episode: 2 });
        assert!(fake.art(&"tt9999901".parse().unwrap(), thumbnail).await.is_ok());
        let missing = [
            fake.art(&"tt0100258".parse().unwrap(), ArtKind::Poster).await,
            fake.art(&"tt9999901".parse().unwrap(), ArtKind::Thumbnail(Episode { season: 1, episode: 3 })).await,
        ];
        assert_eq!(missing.map(|m| m.unwrap_err().code), [ErrorCode::NotFound, ErrorCode::NotFound]);
    }
}
