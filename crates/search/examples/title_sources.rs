//! Shows which releases a title's search keeps and which it drops, against the live sources.
//! Run it to judge the matcher on real names: `cargo run --example title_sources tt0063350`.

use std::collections::BTreeSet;
use std::time::Duration;

use torrential_core::title::{Namesakes, Wanted, normalized};
use torrential_core::{Category, SearchQuery, VideoRef};
use torrential_search::catalog::{Catalog, CatalogConfig};
use torrential_search::{ResultCache, Search, SearchConfig};

const MOST: u16 = 200;
const WITHIN_MS: u32 = 12_000;

#[tokio::main]
async fn main() {
    let Some(video) = std::env::args().nth(1).and_then(|text| text.parse::<VideoRef>().ok()) else {
        eprintln!("usage: title_sources <tt0063350 | tt1748166:1:2>");
        std::process::exit(2);
    };
    let catalog = Catalog::new(&CatalogConfig { online: true, endpoint: None, cache_dir: None }).expect("a catalog");
    let details = catalog.title(&video.title).await.unwrap_or_else(|e| fail(&e.to_string()));
    let same = catalog.namesakes(&details.card).await.unwrap_or_default();
    let namesakes = match same.iter().position(|card| card.id == details.card.id) {
        _ if same.len() < 2 => Namesakes::None,
        Some(0) => Namesakes::Leading,
        _ => Namesakes::Shadowed,
    };
    let season = video.episode.and_then(|e| details.seasons.iter().find(|s| s.number == e.season));
    let wanted = Wanted {
        name: details.card.name.clone(),
        year: details.card.year,
        episode: video.episode,
        namesakes,
        runtime: details.runtime_min.map(|minutes| Duration::from_secs(u64::from(minutes) * 60)),
        season_episodes: season.map(|s| s.episodes.len() as u32),
    };
    println!("{} ({:?}), {namesakes:?}, queries {:?}", wanted.name, wanted.year, wanted.queries());

    let search = Search::new(&SearchConfig::default(), ResultCache::new()).expect("a search");
    let query = |text: String, wanted: Option<Wanted>| SearchQuery {
        category: Some(Category::Video),
        limit: MOST,
        timeout_ms: WITHIN_MS,
        wanted,
        ..SearchQuery::new(text)
    };
    let kept = search.search(&query(wanted.name.clone(), Some(wanted.clone()))).await;
    let kept = kept.unwrap_or_else(|e| fail(&e.to_string()));
    println!("\nkept {} of {} in play order:", kept.releases.len(), kept.total);
    for release in &kept.releases {
        let span = release.span.map(|span| span.to_string()).unwrap_or_default();
        let seeders = release.seeders.map_or("?".into(), |s| s.to_string());
        let size = release.size.map_or("?".into(), |bytes| format!("{:.1}G", bytes as f64 / 1e9));
        println!("  {span:<9}{seeders:>6} seeds {size:>7}  {}  [{}]", release.name, release.sources.join(","));
    }

    let ids: Vec<_> = kept.releases.iter().map(|r| r.id).collect();
    let mut dropped = BTreeSet::new();
    for text in wanted.queries() {
        if let Ok(raw) = search.search(&query(text, None)).await {
            let gone = raw.releases.into_iter().filter(|r| !ids.contains(&r.id));
            dropped.extend(gone.map(|r| (wanted.fit(&r.name).is_some(), r.name, r.seeders)));
        }
    }
    let name = normalized(&wanted.name);
    let (fits, rest): (Vec<_>, Vec<_>) = dropped.into_iter().partition(|(fits, ..)| *fits);
    let (near, far): (Vec<_>, Vec<_>) = rest.into_iter().partition(|(_, d, _)| normalized(d).contains(&name));
    println!("\ndropped {} that fit the title, for a dead swarm or a fake size:", fits.len());
    fits.iter().for_each(|(_, name, seeders)| println!("  {seeders:?} seeds  {name}"));
    println!("\ndropped {} that do not fit, whose name holds the title:", near.len());
    near.iter().for_each(|(_, name, seeders)| println!("  {seeders:?} seeds  {name}"));
    println!("\ndropped {} others", far.len());
}

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(1)
}
