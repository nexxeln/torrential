//! `torrential search`: torrents that match the words.

use std::time::Duration;

use torrential_core::protocol::{Request, Response};
use torrential_core::text::sanitize;
use torrential_core::units::size_text;
use torrential_core::{Arch, Error, Preferences, Profile, SearchQuery, SearchResult, SourceOutcome};

use super::connect::{self, Conn};
use super::rows::{ranked_for, release_ids, release_row};
use super::{answer, emit};
use crate::cli::SearchArgs;
use crate::output::format::{self, source_outcome};
use crate::output::{Body, Doc, Mode};

pub fn run(mut conn: Conn, args: SearchArgs, preferences: Preferences, mode: Mode) -> Result<u8, Error> {
    let query = SearchQuery {
        text: args.words.join(" "),
        sources: args.source,
        category: args.category.map(Into::into),
        min_seeds: args.min_seeds,
        max_size: args.max_size,
        sort: args.sort.into(),
        limit: args.limit,
        timeout_ms: args.timeout.as_millis().min(u128::from(u32::MAX)) as u32,
        adult: args.adult,
        fresh: args.fresh,
        profile: Profile { preferences, ..Profile::default() },
        wanted: None,
    };
    let within = args.timeout + Duration::from_secs(10);
    match answer(conn.call_within(&Request::Search { query, stream: false }, within)?)? {
        Response::Search { result } => {
            let doc = search(&result, torrential_core::time::now());
            Ok(emit(mode, &doc, &Response::Search { result }))
        }
        other => Err(connect::unexpected(&other)),
    }
}

pub fn search(result: &SearchResult, now: i64) -> Doc {
    let ids = release_ids(&result.releases);
    let video = result.is_video();
    let rows = result.releases.iter().zip(&ids).map(|(r, id)| release_row(r, id, video, now)).collect();
    let ok = result.sources.iter().filter(|s| s.outcome.is_ok()).count();
    let found = if result.releases.is_empty() {
        "no results".to_string()
    } else {
        format!("{} of {}", result.releases.len(), format::count(result.total as usize, "result"))
    };
    let mut line = vec![found, format!("{ok} of {} ok", format::count(result.sources.len(), "source"))];
    line.extend(
        result
            .sources
            .iter()
            .filter(|s| !s.outcome.is_ok())
            .map(|s| format!("{}: {}", s.source, source_outcome(&s.outcome))),
    );
    line.push(format::seconds_tenths(result.elapsed_ms));
    if result.hidden_adult > 0 {
        line.push(format!("{} adult hidden: --adult", result.hidden_adult));
    }
    let shown = result.releases.len() as u32;
    if result.total > shown {
        line.push(format!("more: --limit {}", result.total.min(shown + 10)));
    }
    let mut summary = vec![line.join(" · ")];
    let query = &result.query;
    let mut filters = Vec::new();
    for source in &query.sources {
        filters.push(format!("--source {}", format::shell_word(&sanitize(source))));
    }
    if let Some(category) = query.category {
        filters.push(format!("--category {}", category.as_str()));
    }
    if let Some(min) = query.min_seeds {
        filters.push(format!("--min-seeds {min}"));
    }
    if let Some(max) = query.max_size {
        filters.push(format!("--max-size {}", size_text(max)));
    }
    let next = match ids.first() {
        Some(id) => format!("torrential files {id}"),
        None => {
            let words: Vec<String> = query.text.split_whitespace().map(|w| format::shell_word(&sanitize(w))).collect();
            let mut next = format!("torrential search {}", words.join(" "));
            let cached = result.sources.iter().any(|s| matches!(s.outcome, SourceOutcome::Cached { .. }));
            if !filters.is_empty() {
                summary.push(format!("filtered by {}", filters.join(" ")));
            } else if cached {
                next.push_str(" --fresh");
            }
            next
        }
    };
    let header = vec!["ID", "SEEDS", "SIZE", "QUALITY", "AGE", "SOURCE", "FLAGS", "NAME"];
    let profile = &result.query.profile;
    if video {
        summary.push(format!("ranked for {}", ranked_for(profile)));
    } else if result.releases.iter().any(|r| !Arch::named_in(&r.name).is_empty()) {
        summary.push(format!("ranked for {}", profile.describe_arch()));
    }
    Doc { body: Body::Table { header, rows }, summary, next: Some(next) }
}
