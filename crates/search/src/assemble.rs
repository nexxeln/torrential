use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use sha1::{Digest, Sha1};
use torrential_core::error::Details;
use torrential_core::title::{Namesakes, Wanted};
use torrential_core::{
    Arch, Error, ErrorCode, Flag, InfoHash, Language, LocatorId, Profile, Quality, Release, ReleaseId, Resolution,
    SearchQuery, SearchResult, SourceOutcome, SourceReport, Swarm, TorrentId,
};

use crate::classify;
use crate::observation::{Identity, Observation, build_magnet};
use crate::rank::{self, FAKE_MBPS};
use crate::sources::{Failure, SourceMeta};
use crate::terms::{Relevance, Terms};

const MAX_TRACKERS: usize = 32;

#[derive(Clone)]
pub(crate) struct Run {
    pub source: Arc<SourceMeta>,
    pub answer: Answer,
    pub elapsed_ms: u32,
}

#[derive(Clone)]
pub(crate) enum Answer {
    Fresh(Arc<Vec<Observation>>),
    Cached(Arc<Vec<Observation>>),
    Failed(Failure),
}

pub(crate) fn vet(terms: &Terms, rows: Vec<Observation>) -> Result<Vec<Observation>, Failure> {
    let total = rows.len();
    let kept: Vec<Observation> = rows.into_iter().filter(|o| terms.is_relevant(o.name())).collect();
    if (total - kept.len()) * 2 > total { Err(Failure::Irrelevant) } else { Ok(kept) }
}

pub(crate) type Linked = (Release, Option<String>);

pub(crate) fn assemble(
    query: &SearchQuery,
    profile: Profile,
    terms: &Terms,
    runs: &[Run],
    elapsed_ms: u32,
) -> Result<(SearchResult, Vec<Linked>), Error> {
    let (result, linked) = merged(query, profile, terms, runs, elapsed_ms);
    if !result.sources.iter().any(|r| r.outcome.is_ok()) {
        return Err(all_failed(result.sources));
    }
    Ok((result, linked))
}

pub(crate) fn merged(
    query: &SearchQuery,
    profile: Profile,
    terms: &Terms,
    runs: &[Run],
    elapsed_ms: u32,
) -> (SearchResult, Vec<Linked>) {
    let mut runs: Vec<&Run> = runs.iter().collect();
    runs.sort_by(|a, b| (a.source.authority, &a.source.name).cmp(&(b.source.authority, &b.source.name)));
    let sources: Vec<SourceReport> = runs.chunk_by(|a, b| a.source.name == b.source.name).map(report).collect();
    let sightings = runs.iter().flat_map(|run| {
        let rows: &[Observation] = match &run.answer {
            Answer::Fresh(rows) | Answer::Cached(rows) => rows,
            Answer::Failed(_) => &[],
        };
        rows.iter().map(|row| (&*run.source, row))
    });
    let (mut kept, hidden_adult) = filter(merge(sightings, terms), query);
    rank::rank(&mut kept, query.sort, &profile);
    let total = u32::try_from(kept.len()).unwrap_or(u32::MAX);
    kept.truncate(query.limit.into());
    let releases = kept.iter().map(Candidate::shown).collect();
    let query = SearchQuery { profile, ..query.clone() };
    let result = SearchResult { query, releases, total, hidden_adult, sources, elapsed_ms };
    (result, kept.into_iter().map(|c| (c.release, c.private_url)).collect())
}

/// One source's report over its runs, one for each of a title's queries.
fn report(runs: &[&Run]) -> SourceReport {
    let count = |rows: &Vec<Observation>| u32::try_from(rows.len()).unwrap_or(u32::MAX);
    let outcome = |run: &Run| match &run.answer {
        Answer::Fresh(rows) => SourceOutcome::Ok { results: count(rows) },
        Answer::Cached(rows) => SourceOutcome::Cached { results: count(rows) },
        Answer::Failed(failure) => failure.outcome(),
    };
    let first = runs[0];
    let outcome = runs[1..].iter().fold(outcome(first), |sum, run| joined(sum, outcome(run)));
    let elapsed_ms = runs.iter().map(|r| r.elapsed_ms).max().unwrap_or(first.elapsed_ms);
    SourceReport { source: first.source.name.clone(), outcome, elapsed_ms }
}

/// Two runs of one source as one outcome. Their rows add up, fresh wins over cached, and an
/// answer wins over a failure. Of two failures, the first says why.
fn joined(first: SourceOutcome, then: SourceOutcome) -> SourceOutcome {
    match (first, then) {
        (SourceOutcome::Cached { results: a }, SourceOutcome::Cached { results: b }) => {
            SourceOutcome::Cached { results: a.saturating_add(b) }
        }
        (
            SourceOutcome::Ok { results: a } | SourceOutcome::Cached { results: a },
            SourceOutcome::Ok { results: b } | SourceOutcome::Cached { results: b },
        ) => SourceOutcome::Ok { results: a.saturating_add(b) },
        (failed, answered) if !failed.is_ok() && answered.is_ok() => answered,
        (first, _) => first,
    }
}

fn all_failed(sources: Vec<SourceReport>) -> Error {
    let names: Vec<&str> = sources.iter().map(|r| r.source.as_str()).collect();
    Error::new(ErrorCode::ProvidersUnavailable, format!("all sources failed ({})", names.join(", ")))
        .with_hint("check the network connection, or retry with a longer --timeout")
        .with_details(Details::Sources(sources))
}

pub(crate) struct Candidate {
    pub release: Release,
    hash: Option<InfoHash>,
    private_url: Option<String>,
    /// The trackers came from a private source and carry the user's passkey.
    private_trackers: bool,
    /// The page link came from a private source, which may put a passkey in it.
    private_info: bool,
    pub(crate) relevance: Relevance,
    pub(crate) extra_words: u16,
    /// A trailer, a clip, or another extra, by a word the query does not hold.
    extra: bool,
    pub(crate) downloads: Option<u64>,
    pub(crate) lawful: bool,
    pub(crate) quality: Quality,
    pub(crate) resolution: Option<Resolution>,
    pub(crate) arches: Vec<Arch>,
    pub(crate) languages: Vec<Language>,
    /// Set only in a title's search, for a release of the wanted title.
    pub(crate) title: Option<TitleFit>,
}

/// How a release of the wanted title plays here. Its span is in `release.span`.
pub(crate) struct TitleFit {
    pub(crate) swarm: Swarm,
    /// Other titles share the name and this release names no year.
    pub(crate) undated: bool,
    /// Mbit/s per film or episode. `None` when the size or the runtime is unknown.
    pub(crate) mbps: Option<f64>,
    /// Words in the name beyond the title, its year, and tags, as an alternate version has.
    pub(crate) other_words: usize,
}

impl Candidate {
    fn shown(&self) -> Release {
        let mut release = self.release.clone();
        if self.private_url.is_some() {
            release.torrent_url = None;
        }
        if self.private_trackers {
            release.magnet = self.hash.map(|h| build_magnet(&h, &release.name, &[]));
        }
        if self.private_info {
            release.info_url = None;
        }
        release
    }
}

type Sighting<'a> = (&'a SourceMeta, &'a Observation);

fn merge<'a>(sightings: impl Iterator<Item = Sighting<'a>>, terms: &Terms) -> Vec<Candidate> {
    let sightings: Vec<Sighting<'a>> = sightings.collect();
    let mut aliases: HashMap<TorrentId, TorrentId> = HashMap::new();
    for (_, o) in &sightings {
        if let Identity::Hash { hash: InfoHash::Hybrid { v1, v2 }, .. } = o.identity {
            let v1 = TorrentId::from_bytes(v1);
            aliases.entry(InfoHash::V2(v2).id()).and_modify(|known| *known = (*known).min(v1)).or_insert(v1);
        }
    }
    let mut groups: HashMap<ReleaseId, Vec<Sighting<'a>>> = HashMap::new();
    for sighting in sightings {
        let id = match &sighting.1.identity {
            Identity::Hash { hash, .. } => {
                let id = hash.id();
                ReleaseId::Torrent(aliases.get(&id).copied().unwrap_or(id))
            }
            Identity::Locator { torrent_url } => {
                ReleaseId::Locator(LocatorId::from_bytes(Sha1::digest(torrent_url.as_bytes()).into()))
            }
        };
        groups.entry(id).or_default().push(sighting);
    }
    groups.into_iter().map(|(id, group)| combine(id, &group, terms)).collect()
}

fn combine(id: ReleaseId, group: &[Sighting<'_>], terms: &Terms) -> Candidate {
    fn first<T: Ord + Clone>(group: &[Sighting<'_>], field: impl Fn(&Observation) -> Option<T>) -> Option<T> {
        group.iter().filter_map(|(s, o)| field(o).map(|v| (s.authority, v))).min().map(|(_, v)| v)
    }
    let name = group
        .iter()
        .map(|(s, o)| {
            // Indexes cut long titles and mark the cut with an ellipsis, which
            // makes the cut name longer than the whole one.
            let name = o.name();
            let whole = name.strip_suffix("...").or_else(|| name.strip_suffix('…')).map(str::trim_end);
            (std::cmp::Reverse(whole.unwrap_or(name).chars().count()), whole.is_some(), s.authority, name)
        })
        .min()
        .map(|(.., name)| name.to_string())
        .unwrap_or_default();
    let category = first(group, |o| o.category);
    let peers = group.iter().filter_map(|(_, o)| o.peers).max();
    let mut flags: BTreeSet<Flag> = group.iter().flat_map(|(_, o)| o.flags.iter().copied()).collect();
    if group.iter().any(|(_, o)| classify::is_cam(o.name(), category)) {
        flags.insert(Flag::Cam);
    }
    if group.iter().any(|(_, o)| classify::is_adult(o.name())) {
        flags.insert(Flag::Adult);
    }
    let mut authorities: Vec<(u16, &str)> = group.iter().map(|(s, _)| (s.authority, s.name.as_str())).collect();
    authorities.sort();
    authorities.dedup();
    let any_public = group.iter().any(|(s, _)| s.public);
    let trackers: BTreeSet<&String> =
        group.iter().filter(|(s, _)| s.public || !any_public).flat_map(|(_, o)| &o.trackers).collect();
    let trackers: Vec<String> = trackers.into_iter().take(MAX_TRACKERS).cloned().collect();
    let hash = combined_hash(group);
    let link =
        group.iter().filter_map(|(s, o)| o.torrent_url().map(|url| (s.authority, !s.public, url, &s.name))).min();
    let info = group.iter().filter_map(|(s, o)| o.info_url.as_deref().map(|url| (s.authority, !s.public, url))).min();
    let relevance = group.iter().map(|(_, o)| terms.relevance(o.name())).max().unwrap_or(terms.relevance(""));
    let extra_words = terms.extra_words(&name);
    let quality = Quality::parse(&name);
    let size = first(group, |o| o.size);
    let extra = classify::is_extra(&name, terms);
    let resolution = quality.resolution.filter(|r| !extra && !rank::is_too_small(size, *r));
    let arches = Arch::named_in(&name);
    let languages = Language::named_in(&name, terms.words());
    Candidate {
        hash,
        private_url: link.filter(|(_, private, ..)| *private).map(|(.., source)| source.clone()),
        private_trackers: !any_public && !trackers.is_empty(),
        private_info: info.is_some_and(|(_, private, _)| private),
        release: Release {
            id,
            magnet: hash.map(|h| build_magnet(&h, &name, &trackers)),
            name,
            size,
            seeders: peers.map(|p| p.seeders),
            leechers: peers.and_then(|p| p.leechers),
            published: first(group, |o| o.published),
            category,
            flags,
            sources: authorities.into_iter().map(|(_, name)| name.to_string()).collect(),
            torrent_url: link.map(|(_, _, url, _)| url.to_string()),
            info_url: info.map(|(.., url)| url.to_string()),
            span: None,
            swarm: None,
            failed: None,
        },
        relevance,
        extra_words,
        extra,
        downloads: group.iter().filter_map(|(_, o)| o.downloads).max(),
        lawful: group.iter().any(|(s, _)| s.lawful),
        quality,
        resolution,
        arches,
        languages,
        title: None,
    }
}

fn combined_hash(group: &[Sighting<'_>]) -> Option<InfoHash> {
    let hashes = group.iter().filter_map(|(_, o)| match &o.identity {
        Identity::Hash { hash, .. } => Some(*hash),
        Identity::Locator { .. } => None,
    });
    let (v1, v2) = hashes.fold((None, None), |(v1, v2), h| (v1.max(h.v1()), v2.max(h.v2())));
    match (v1, v2) {
        (Some(v1), Some(v2)) => Some(InfoHash::Hybrid { v1, v2 }),
        (Some(v1), None) => Some(InfoHash::V1(v1)),
        (None, Some(v2)) => Some(InfoHash::V2(v2)),
        (None, None) => None,
    }
}

fn filter(candidates: Vec<Candidate>, q: &SearchQuery) -> (Vec<Candidate>, u32) {
    let mut hidden_adult = 0;
    let kept = candidates
        .into_iter()
        .filter(|c| {
            let r = &c.release;
            q.category.is_none_or(|want| r.category == Some(want))
                && q.min_seeds.is_none_or(|min| r.seeders.is_some_and(|s| s >= min))
                && q.max_size.is_none_or(|max| r.size.is_some_and(|s| s <= max))
        })
        .filter_map(|c| match &q.wanted {
            Some(wanted) => fitted(c, wanted),
            None => Some(c),
        })
        .filter(|c| {
            let hide = !q.adult && c.release.flags.contains(&Flag::Adult);
            hidden_adult += u32::from(hide);
            !hide
        })
        .collect();
    (kept, hidden_adult)
}

/// The candidate as a release of the wanted title, or `None` when it is another title or an
/// extra such as its trailer, when its swarm is known to be dead, or when its bitrate says it
/// is a fake.
fn fitted(mut c: Candidate, wanted: &Wanted) -> Option<Candidate> {
    if c.extra {
        return None;
    }
    let reading = wanted.fit(&c.release.name)?;
    let swarm = rank::swarm(c.release.seeders, c.lawful)?;
    let bitrate = rank::bitrate(c.release.size, reading.span, wanted);
    if bitrate.is_some_and(|mbps| mbps < FAKE_MBPS) {
        return None;
    }
    c.release.span = Some(reading.span);
    c.release.swarm = Some(swarm);
    c.title = Some(TitleFit {
        swarm,
        undated: wanted.namesakes == Namesakes::Leading && reading.year.is_none(),
        mbps: bitrate,
        other_words: wanted.other_words(&c.release.name),
    });
    Some(c)
}

#[cfg(test)]
mod tests;
