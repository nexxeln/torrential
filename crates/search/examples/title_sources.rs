//! Shows which releases a title's search keeps and which it drops, against the live sources.
//! Run it to judge the matcher on real names: `cargo run --example title_sources tt0063350`.
//! `--screen 1080p` or `--screen 2160p` and `--language en` rank as a machine with that screen
//! and system language would.

use std::collections::BTreeSet;
use std::time::Duration;

use torrential_core::title::{Namesakes, Span, Wanted, normalized};
use torrential_core::{Category, Language, Machine, Os, Preferences, Profile, Screen, SearchQuery, VideoRef};
use torrential_search::catalog::{Catalog, CatalogConfig};
use torrential_search::{ResultCache, Search, SearchConfig};

const MOST: u16 = 200;
const WITHIN_MS: u32 = 12_000;

#[tokio::main]
async fn main() {
    let (video, profile) = arguments().unwrap_or_else(|| {
        eprintln!("usage: title_sources <tt0063350 | tt1748166:1:2> [--screen 1080p|2160p] [--language en]");
        std::process::exit(2);
    });
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
    println!("ranked for {}", profile.describe());

    let search = Search::new(&SearchConfig::default(), ResultCache::new()).expect("a search");
    let query = |text: String, wanted: Option<Wanted>| SearchQuery {
        category: Some(Category::Video),
        limit: MOST,
        timeout_ms: WITHIN_MS,
        profile: profile.clone(),
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
        let words = wanted.other_words(&release.name);
        println!("  {span:<9}{seeders:>6} seeds {size:>7} +{words}  {}  [{}]", release.name, release.sources.join(","));
    }

    let ids: Vec<_> = kept.releases.iter().map(|r| r.id).collect();
    let mut dropped = BTreeSet::new();
    for text in wanted.queries() {
        if let Ok(raw) = search.search(&query(text, None)).await {
            let gone = raw.releases.into_iter().filter(|r| !ids.contains(&r.id));
            dropped.extend(gone.map(|r| (wanted.fit(&r.name).map(|n| n.span), r.name, r.seeders, r.size)));
        }
    }
    let name = normalized(&wanted.name);
    let (fits, rest): (Vec<_>, Vec<_>) = dropped.into_iter().partition(|(span, ..)| span.is_some());
    let (near, far): (Vec<_>, Vec<_>) = rest.into_iter().partition(|(_, d, ..)| normalized(d).contains(&name));
    println!("\ndropped {} that fit the title, for no seeders, a fake size, or being an extra:", fits.len());
    for (span, name, seeders, size) in &fits {
        let reason = match (seeders, span.and_then(|span| mbps(&wanted, span, *size))) {
            (Some(0), _) => "no seeders".to_string(),
            (_, Some(rate)) if rate < 0.25 => format!("{rate:.2} Mbit/s"),
            _ => "an extra".to_string(),
        };
        println!("  {reason:<12}  {name}");
    }
    println!("\ndropped {} that do not fit, whose name holds the title:", near.len());
    near.iter().for_each(|(_, name, seeders, _)| println!("  {seeders:?} seeds  {name}"));
    println!("\ndropped {} others", far.len());
}

/// Mbit/s per film or episode, as the search works it out to drop a fake.
fn mbps(wanted: &Wanted, span: Span, size: Option<u64>) -> Option<f64> {
    let episodes = match span {
        Span::Film | Span::Episode(_) => 1,
        Span::Season(_) => wanted.season_episodes?,
        Span::Seasons { first, last } => wanted.season_episodes? * (last - first + 1),
        Span::Complete => return None,
    };
    Some(size? as f64 * 8.0 / (wanted.runtime?.as_secs_f64() * f64::from(episodes)) / 1e6)
}

/// The video, and a profile with the screen and language the flags name.
fn arguments() -> Option<(VideoRef, Profile)> {
    let mut video = None;
    let mut profile = Profile::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--screen" => {
                let screen = match args.next()?.as_str() {
                    "1080p" => Screen::Tv,
                    "2160p" => Screen::Tv4k,
                    _ => return None,
                };
                profile.preferences = Preferences { screen: Some(screen), ..profile.preferences };
            }
            "--language" => {
                let language = Some(Language::from_locale(&args.next()?)?);
                profile.detected = Some(Machine { os: Os::of_host(), display: None, arch: None, language });
            }
            text => video = Some(text.parse::<VideoRef>().ok()?),
        }
    }
    Some((video?, profile))
}

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(1)
}
