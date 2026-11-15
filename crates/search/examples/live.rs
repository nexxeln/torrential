//! Searches the live sources for "ubuntu" over the network and prints what each answered.
//! Run with `cargo run -p torrential-search --example live`.

use torrential_core::{SearchQuery, SourceOutcome};
use torrential_search::{ResultCache, Search, SearchConfig};

#[tokio::main]
async fn main() {
    let search = Search::new(&SearchConfig::default(), ResultCache::new()).unwrap();
    let result = search.search(&SearchQuery::new("ubuntu")).await.unwrap();

    println!("{:<18} {:<14} {:>7} {:>8}", "SOURCE", "OUTCOME", "RESULTS", "ELAPSED");
    for report in &result.sources {
        let (outcome, results) = match &report.outcome {
            SourceOutcome::Ok { results } => ("ok".to_string(), results.to_string()),
            SourceOutcome::Cached { results } => ("cached".to_string(), results.to_string()),
            other => (format!("{other:?}"), "-".to_string()),
        };
        println!("{:<18} {:<14} {:>7} {:>6}ms", report.source, outcome, results, report.elapsed_ms);
    }
    println!("# {} of {} results in {} ms", result.releases.len(), result.total, result.elapsed_ms);
    for release in &result.releases {
        let flags: Vec<&str> = release.flags.iter().map(|f| f.as_str()).collect();
        println!(
            "{}\t{}\t{}\t{}\t{}",
            &release.id.to_ref()[..8],
            release.seeders.map_or("-".into(), |s| s.to_string()),
            release.sources.join("+"),
            flags.join(","),
            release.name
        );
    }

    let answered = result.sources.iter().filter(|r| matches!(r.outcome, SourceOutcome::Ok { results } if results > 0));
    assert!(answered.count() >= 4, "fewer than four sources returned results");
    assert!(result.elapsed_ms <= 6_200, "took {} ms", result.elapsed_ms);
}
