//! What the title commands print: films and series, one title, its sources, and the steps of a
//! title play.

use torrential_core::protocol::{GetReply, TitlePage, TitleSources};
use torrential_core::text::sanitize;
use torrential_core::title::Episode;
use torrential_core::{AttemptEvent, Check, Kind, Release, Skip, TitleCard, Watched};

use super::super::format::{self, NONE, UNKNOWN};
use super::super::theme::Token;
use super::super::{Body, Cell, Doc};
use super::{get, ranked_for, release_ids, release_row, short_ids, short_ref, source_outcome};
use crate::daemon::titles::catalog_port::label;

/// A film's name and year, or a series' name and the episode, as `Harbor Lights S01E02`.
pub fn title_name(card: &TitleCard, episode: Option<Episode>) -> String {
    match (episode, card.year) {
        (Some(episode), _) => format!("{} {}", card.name, label(episode)),
        (None, Some(year)) => format!("{} ({year})", card.name),
        (None, None) => card.name.clone(),
    }
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Film => "film",
        Kind::Series => "series",
    }
}

fn rating(rating: Option<f32>) -> String {
    rating.map_or_else(|| UNKNOWN.to_string(), |r| format!("{r:.1}"))
}

/// The torrent search for `words`, such as a title's name and year.
pub fn search_for(words: &str) -> String {
    let words: Vec<String> = words
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|w| !w.is_empty())
        .map(|w| format::shell_word(&sanitize(w)))
        .collect();
    format!("torrential search {}", words.join(" "))
}

/// Films and series named like `words`, as `torrential titles` lists them.
pub fn titles(titles: &[TitleCard], words: &str) -> Doc {
    let rows = titles
        .iter()
        .map(|t| {
            vec![
                Cell::styled(t.id.as_str(), Token::Accent),
                Cell::new(kind(t.kind)),
                Cell::new(t.year.map_or_else(|| UNKNOWN.to_string(), |y| y.to_string())),
                Cell::new(rating(t.rating)),
                Cell::new(&t.name),
            ]
        })
        .collect();
    let (summary, next) = match titles.first() {
        Some(first) => (format::count(titles.len(), "title"), format!("torrential title {}", first.id)),
        None => ("no film or series has that name".to_string(), search_for(words)),
    };
    let header = vec!["ID", "KIND", "YEAR", "RATING", "NAME"];
    Doc { body: Body::Table { header, rows }, summary: vec![summary], next: Some(next) }
}

/// One film as fields, or one series as its episodes, with where the person stopped.
pub fn title(page: &TitlePage) -> Doc {
    let card = &page.details.card;
    let next = match &page.next {
        Some(next) => format!("torrential play {}", next.video),
        None => format!("torrential play {}", card.id),
    };
    let body = match card.kind {
        Kind::Film => film(page),
        Kind::Series => episodes(page),
    };
    let mut summary = Vec::new();
    if card.kind == Kind::Series {
        summary.push(about(page).join(" · "));
        summary.extend(page.details.synopsis.as_ref().map(|s| sanitize(s)));
        if !page.details.cast.is_empty() {
            summary.push(format!("cast: {}", page.details.cast.join(", ")));
        }
        if !page.downloads.is_empty() {
            summary.push(format!("downloads: {}", downloads(page)));
        }
        summary.push(up_next(page));
    }
    Doc { body, summary, next: Some(next) }
}

fn about(page: &TitlePage) -> Vec<String> {
    let details = &page.details;
    let mut about =
        vec![format!("{} {}", details.card.id, title_name(&details.card, None)), kind(details.card.kind).into()];
    if let Some(r) = details.card.rating {
        about.push(format!("rated {}", rating(Some(r))));
    }
    if !details.genres.is_empty() {
        about.push(details.genres.join(", "));
    }
    if let Some(minutes) = details.runtime_min {
        about.push(format!("{minutes} min"));
    }
    about
}

fn film(page: &TitlePage) -> Body {
    let details = &page.details;
    let title = vec![Cell::styled(details.card.id.as_str(), Token::Accent), Cell::new(title_name(&details.card, None))];
    let mut fields = vec![("kind", Cell::new("film")), ("rating", Cell::new(rating(details.card.rating)))];
    if let Some(minutes) = details.runtime_min {
        fields.push(("runtime", Cell::new(format!("{minutes} min"))));
    }
    if !details.genres.is_empty() {
        fields.push(("genres", Cell::new(details.genres.join(", "))));
    }
    if !details.cast.is_empty() {
        fields.push(("cast", Cell::new(details.cast.join(", "))));
    }
    if let Some(watched) = page.watched.iter().find(|w| w.video.episode.is_none()) {
        fields.push(("watched", Cell::new(stopped(watched))));
    }
    if !page.downloads.is_empty() {
        fields.push(("downloads", Cell::new(downloads(page))));
    }
    if let Some(synopsis) = &details.synopsis {
        fields.push(("about", Cell::new(synopsis)));
    }
    Body::Fields { title, fields }
}

fn episodes(page: &TitlePage) -> Body {
    let rows = page
        .details
        .seasons
        .iter()
        .flat_map(|season| &season.episodes)
        .map(|info| {
            let watched = page.watched.iter().find(|w| w.video.episode == Some(info.episode));
            vec![
                Cell::styled(label(info.episode), Token::Accent),
                Cell::new(info.aired.map_or_else(|| UNKNOWN.to_string(), |t| format::timestamp(t)[..10].to_string())),
                match watched {
                    Some(w) if w.finished() => Cell::styled("finished", Token::Mint),
                    Some(w) => Cell::new(clock(w.position)),
                    None => Cell::styled(NONE, Token::Muted),
                },
                Cell::new(info.name.as_deref().unwrap_or(UNKNOWN)),
            ]
        })
        .collect();
    Body::Table { header: vec!["EPISODE", "AIRED", "WATCHED", "NAME"], rows }
}

fn clock(seconds: f64) -> String {
    format::clock(std::time::Duration::from_secs_f64(seconds.max(0.0)))
}

fn stopped(watched: &Watched) -> String {
    match watched.duration {
        _ if watched.finished() => "finished".to_string(),
        Some(duration) => format!("stopped at {} of {}", clock(watched.position), clock(duration)),
        None => format!("stopped at {}", clock(watched.position)),
    }
}

fn downloads(page: &TitlePage) -> String {
    let ids = short_ids(&page.downloads.iter().map(|j| j.id).collect::<Vec<_>>());
    page.downloads.iter().zip(ids).map(|(job, id)| format!("{id} {}", job.status.name())).collect::<Vec<_>>().join(", ")
}

fn up_next(page: &TitlePage) -> String {
    let Some(next) = &page.next else { return "caught up, so play starts from the first episode".to_string() };
    let Some(episode) = next.video.episode else { return String::new() };
    let info = page.details.seasons.iter().flat_map(|s| &s.episodes).find(|info| info.episode == episode);
    let mut line = format!("up next: {}", label(episode));
    if let Some(name) = info.and_then(|info| info.name.as_deref()) {
        line.push(' ');
        line.push_str(name);
    }
    if let Some(at) = next.resume_at {
        line.push_str(&format!(", from {}", clock(at)));
    }
    line
}

/// A film's or an episode's sources, in the order a play tries them.
pub fn sources(sources: &TitleSources, now: i64) -> Doc {
    let result = &sources.result;
    let ids = release_ids(&result.releases);
    let episode = sources.video.episode.is_some();
    let rows = result
        .releases
        .iter()
        .zip(&ids)
        .map(|(r, id)| {
            let mut row = release_row(r, id, true, now);
            if episode {
                let span = r.span.map_or_else(|| NONE.to_string(), |span| span.to_string());
                row.insert(row.len() - 1, Cell::new(span));
            }
            row
        })
        .collect();
    let mut header = vec!["ID", "SEEDS", "SIZE", "QUALITY", "AGE", "SOURCE", "FLAGS", "NAME"];
    if episode {
        header.insert(header.len() - 1, "HOLDS");
    }
    let name = title_name(&sources.title, sources.video.episode);
    let found = match result.releases.len() {
        0 => format!("no source of {name} was found"),
        n => format!("{} of {name}, in the order play tries them", format::count(n, "source")),
    };
    let mut summary = vec![format!("{found} · {}", format::seconds_tenths(result.elapsed_ms))];
    let failed: Vec<String> = result
        .sources
        .iter()
        .filter(|s| !s.outcome.is_ok())
        .map(|s| format!("{}: {}", s.source, source_outcome(&s.outcome)))
        .collect();
    if !failed.is_empty() {
        summary.push(failed.join(" · "));
    }
    for (release, id) in result.releases.iter().zip(&ids) {
        if let Some(why) = &release.failed {
            summary.push(format!("{id} failed lately, so play tries it last: {}", skip(why)));
        }
    }
    summary.push(format!("ranked for {}", ranked_for(&result.query.profile)));
    let next = if result.releases.is_empty() {
        search_for(&name)
    } else {
        summary.push(format!("to play only one: torrential play {} --source <ID>", sources.video));
        format!("torrential play {}", sources.video)
    };
    Doc { body: Body::Table { header, rows }, summary, next: Some(next) }
}

/// A title play's step as a line, or `None` for a step a person need not see. `started` names
/// what happens to the source that starts, such as `Playing`.
pub fn step(step: &AttemptEvent, started: &str) -> Option<String> {
    match step {
        AttemptEvent::Resolved { video, title, .. } => {
            Some(format!("Finding sources of {}", title_name(title, video.episode)))
        }
        AttemptEvent::Searched { found, .. } => Some(format!("Found {}", format::count(*found as usize, "source"))),
        AttemptEvent::Trying { rank, of, release } => Some(format!("Trying {rank} of {of}: {}", described(release))),
        AttemptEvent::Skipped { release, reason } => {
            Some(format!("Skipped {}: {}", short_ref(&release.to_ref()), skip(reason)))
        }
        AttemptEvent::Playing { release, .. } => Some(format!("{started} {}", described(release))),
        AttemptEvent::Check { .. } | AttemptEvent::Failed { .. } => None,
    }
}

/// What a title play waits for while it checks a source.
pub fn check(check: Check) -> &'static str {
    match check {
        Check::FileList => "Waiting for its file list",
        Check::Start => "Starting the download",
        Check::FirstBytes => "Waiting for the first bytes of the video",
    }
}

fn described(release: &Release) -> String {
    let mut parts = vec![format!("{} {}", short_ref(&release.id.to_ref()), sanitize(&release.name))];
    let quality = release.quality().to_string();
    if !quality.is_empty() {
        parts.push(quality);
    }
    parts.push(format::size(release.size));
    parts.join(", ")
}

/// Why a title play moved past a source, in plain words.
fn skip(reason: &Skip) -> String {
    match reason {
        Skip::NoFileList { waited_s } => format!("no peer sent its file list in {waited_s}s"),
        Skip::NoVideo => "none of its files is the video".to_string(),
        Skip::Unplayable { container } => format!("its .{container} file needs ffmpeg, which is not installed"),
        Skip::TooLarge { error } | Skip::Failed { error } => sanitize(&error.message),
        Skip::NoData { waited_s } => format!("no data arrived in {waited_s}s"),
        Skip::Stalled => "it stalled".to_string(),
    }
}

/// A download a title play started, as `get` shows one, under the title's name.
pub fn get_title(reply: &GetReply, name: &str) -> Doc {
    let mut doc = get(reply, None);
    if let Body::Fields { fields, .. } = &mut doc.body {
        fields.insert(0, ("title", Cell::new(name)));
    }
    doc
}
