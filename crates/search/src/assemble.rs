use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeSet, HashMap};
use std::ops::RangeInclusive;
use std::sync::Arc;

use sha1::{Digest, Sha1};
use torrential_core::error::Details;
use torrential_core::title::{Namesakes, Span, Wanted};
use torrential_core::{
    Arch, Category, Error, ErrorCode, Flag, InfoHash, Language, LocatorId, Profile, Quality, Release, ReleaseId,
    Resolution, SearchQuery, SearchResult, Sort, SourceOutcome, SourceReport, TorrentId,
};

use crate::classify;
use crate::observation::{Identity, Observation, build_magnet};
use crate::sources::{Failure, SourceMeta};
use crate::terms::{self, Relevance, Terms};

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
    rank(&mut kept, query.sort, &profile);
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
    relevance: Relevance,
    extra_words: u16,
    /// A trailer, a clip, or another extra, by a word the query does not hold.
    extra: bool,
    downloads: Option<u64>,
    lawful: bool,
    quality: Quality,
    resolution: Option<Resolution>,
    arches: Vec<Arch>,
    languages: Vec<Language>,
    /// Set only in a title's search, for a release of the wanted title.
    title: Option<TitleFit>,
}

/// How a release of the wanted title plays here. Its span is in `release.span`.
struct TitleFit {
    swarm: Swarm,
    /// Other titles share the name and this release names no year.
    undated: bool,
    /// The bitrate is in the usual band for its resolution, or unknown.
    in_band: bool,
    /// Words in the name beyond the title, its year, and tags, as an alternate version has.
    other_words: usize,
}

/// How likely a swarm is to send data now. Lower plays first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Swarm {
    Strong,
    Fair,
    Weak,
    Unknown,
}

impl Swarm {
    /// `None` for a swarm known to have no seeder outside a lawful catalog. A lawful catalog that
    /// does not count seeders serves its files from web seeds over HTTP, as dependable as any swarm.
    fn of(seeders: Option<u32>, lawful: bool) -> Option<Swarm> {
        match (seeders, lawful) {
            (Some(20..), _) | (None, true) => Some(Swarm::Strong),
            (Some(5..=19), _) => Some(Swarm::Fair),
            (Some(1..=4), _) => Some(Swarm::Weak),
            (Some(0), true) | (None, false) => Some(Swarm::Unknown),
            (Some(0), false) => None,
        }
    }
}

/// Below this many Mbit/s a release is a fake or a broken file.
const FAKE_MBPS: f64 = 0.25;

/// The Mbit/s at each resolution that looks right and streams comfortably. A small encode of a
/// popular release sits low in it, and a remux runs above it.
fn band(resolution: Option<Resolution>) -> RangeInclusive<f64> {
    match resolution {
        Some(Resolution::Sd) => 0.3..=4.0,
        Some(Resolution::P720) => 0.6..=8.0,
        Some(Resolution::P1080) => 1.0..=16.0,
        Some(Resolution::P2160) => 3.0..=40.0,
        None => 0.3..=16.0,
    }
}

/// Mbit/s per film or episode, from the size and the catalog's runtime. `None` when either is
/// unknown, and for a release of every season, whose episodes are not counted.
fn bitrate(size: Option<u64>, span: Span, wanted: &Wanted) -> Option<f64> {
    let episodes = match span {
        Span::Film | Span::Episode(_) => 1,
        Span::Season(_) => wanted.season_episodes?,
        Span::Seasons { first, last } => wanted.season_episodes? * (last - first + 1),
        Span::Complete => return None,
    };
    let seconds = wanted.runtime?.as_secs_f64() * f64::from(episodes);
    (seconds > 0.0).then(|| size.map(|size| size as f64 * 8.0 / seconds / 1e6))?
}

/// Play preference for a release of the wanted title. Derived `Ord`, so lower plays first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PlayRank {
    cam: bool,
    unfit: bool,
    swarm: Swarm,
    undated: bool,
    standing: Reverse<u8>,
    other_words: usize,
    span: u8,
    closeness: u8,
    off_band: bool,
    /// Most first. `Reverse` puts unknown after every count.
    seeders: Reverse<Option<u32>>,
}

/// How much more than the film or episode a release holds: none, a season, several, or all.
fn breadth(span: Option<Span>) -> u8 {
    match span {
        None | Some(Span::Film | Span::Episode(_)) => 0,
        Some(Span::Season(_)) => 1,
        Some(Span::Seasons { .. }) => 2,
        Some(Span::Complete) => 3,
    }
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
    let resolution = quality.resolution.filter(|r| !extra && !classify::is_too_small(size, *r));
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
    let swarm = Swarm::of(c.release.seeders, c.lawful)?;
    let bitrate = bitrate(c.release.size, reading.span, wanted);
    if bitrate.is_some_and(|mbps| mbps < FAKE_MBPS) {
        return None;
    }
    c.release.span = Some(reading.span);
    c.title = Some(TitleFit {
        swarm,
        undated: wanted.namesakes == Namesakes::Leading && reading.year.is_none(),
        in_band: bitrate.is_none_or(|mbps| band(c.quality.resolution).contains(&mbps)),
        other_words: wanted.other_words(&c.release.name),
    });
    Some(c)
}

/// In a title's search the play order leads: not a cam, fits, swarm, names the year, standing,
/// a plain name, span, screen, bitrate, and seeders. The chain after it orders every search.
fn rank(candidates: &mut [Candidate], sort: Sort, profile: &Profile) {
    let fits = |c: &Candidate| profile.fits(c.release.size, c.quality);
    let is_video = |c: &Candidate| c.release.category == Some(Category::Video) || c.quality.resolution.is_some();
    let video_search = candidates.iter().filter(|c| is_video(c)).count() * 2 > candidates.len();
    let closeness = |c: &Candidate| if video_search { profile.closeness(c.resolution) } else { 0 };
    let play = |c: &Candidate| {
        c.title.as_ref().map(|t| PlayRank {
            cam: c.release.flags.contains(&Flag::Cam),
            unfit: !fits(c),
            swarm: t.swarm,
            undated: t.undated,
            standing: Reverse(standing(c)),
            other_words: t.other_words,
            span: breadth(c.release.span),
            closeness: profile.closeness(c.resolution),
            off_band: !t.in_band,
            seeders: Reverse(c.release.seeders),
        })
    };
    candidates.sort_by(|a, b| {
        let (ra, rb) = (&a.release, &b.release);
        let primary = play(a).cmp(&play(b)).then(match sort {
            Sort::Relevance => Ordering::Equal,
            Sort::Seeds => known_first_desc(ra.seeders, rb.seeders),
            Sort::Size => known_first_desc(ra.size, rb.size),
            Sort::Age => known_first_desc(ra.published, rb.published),
        });
        primary
            .then_with(|| b.relevance.cmp(&a.relevance))
            .then_with(|| fits(b).cmp(&fits(a)))
            .then_with(|| standing(b).cmp(&standing(a)))
            .then_with(|| closeness(a).cmp(&closeness(b)))
            .then_with(|| profile.arch_rank(&a.arches).cmp(&profile.arch_rank(&b.arches)))
            .then_with(|| profile.language_rank(&a.languages).cmp(&profile.language_rank(&b.languages)))
            .then_with(|| known_first_desc(ra.seeders, rb.seeders))
            .then_with(|| a.extra_words.cmp(&b.extra_words))
            .then_with(|| known_first_desc(a.downloads, b.downloads))
            .then_with(|| terms::newer_first(&ra.name, &rb.name))
            .then_with(|| ra.id.to_ref().cmp(&rb.id.to_ref()))
    });
}

fn standing(c: &Candidate) -> u8 {
    if c.release.flags.contains(&Flag::Official) { 2 } else { u8::from(c.lawful) }
}

fn known_first_desc<T: Ord>(a: Option<T>, b: Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests;
