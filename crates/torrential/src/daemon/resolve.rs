use torrential_core::error::{Candidate, Details};
use torrential_core::id::shortest_unique_prefix;
use torrential_core::{Error, ErrorCode, HexPrefix, Release, ReleaseId, TorrentId};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Found<'a> {
    Job(TorrentId),
    Release(&'a Release),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Downloads,
    DownloadsAndResults,
}

pub fn prefix<'a>(
    prefix: &HexPrefix,
    jobs: &[(TorrentId, Option<&str>)],
    results: &'a [Release],
    scope: Scope,
    command: &str,
) -> Result<Found<'a>, Error> {
    let mut found: Vec<(ReleaseId, Found<'a>, String, &'static str)> = Vec::new();
    for (id, name) in jobs {
        let rid = ReleaseId::Torrent(*id);
        if prefix.matches(&rid) {
            found.push((rid, Found::Job(*id), name.unwrap_or("?").to_string(), "download"));
        }
    }
    if scope == Scope::DownloadsAndResults {
        for release in results {
            if prefix.matches(&release.id) && !found.iter().any(|(rid, ..)| *rid == release.id) {
                found.push((release.id, Found::Release(release), release.name.clone(), "search result"));
            }
        }
    }
    let typed = if prefix.is_locator() { format!("u{}", prefix.as_str()) } else { prefix.as_str().to_string() };
    match found.len() {
        0 => {
            let error = match scope {
                Scope::Downloads => Error::new(ErrorCode::NotFound, format!("no download matches {typed}"))
                    .with_hint("run torrential status to list downloads")
                    .with_next("torrential status"),
                Scope::DownloadsAndResults => {
                    Error::new(ErrorCode::NotFound, format!("no download or recent search result matches {typed}"))
                        .with_hint("run torrential search <words> to find it, or pass a magnet link")
                }
            };
            Err(error)
        }
        1 => Ok(found.remove(0).1),
        n => {
            let ids: Vec<TorrentId> = found.iter().map(|(rid, ..)| as_torrent_id(rid)).collect();
            let candidates: Vec<Candidate> = found
                .iter()
                .zip(&ids)
                .map(|((rid, _, name, kind), id)| Candidate {
                    id: format!(
                        "{}{}",
                        if matches!(rid, ReleaseId::Locator(_)) { "u" } else { "" },
                        shortest_unique_prefix(id, &ids, 6)
                    ),
                    name: name.clone(),
                    kind: kind.to_string(),
                })
                .collect();
            let first = candidates[0].id.clone();
            Err(Error::new(
                ErrorCode::AmbiguousRef,
                format!("{typed} matches {}", crate::output::format::count(n, "torrent")),
            )
            .with_details(Details::Candidates(candidates))
            .with_hint(format!("use a longer prefix, such as: torrential {command} {first}")))
        }
    }
}

fn as_torrent_id(id: &ReleaseId) -> TorrentId {
    match id {
        ReleaseId::Torrent(t) => *t,
        ReleaseId::Locator(l) => l.to_hex().parse().expect("locator IDs are 40 hex characters"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn tid(hex_prefix: &str) -> TorrentId {
        format!("{hex_prefix}{}", "0".repeat(40 - hex_prefix.len())).parse().unwrap()
    }

    fn release(id: TorrentId, name: &str) -> Release {
        Release {
            id: ReleaseId::Torrent(id),
            name: name.into(),
            size: None,
            seeders: None,
            leechers: None,
            published: None,
            category: None,
            flags: BTreeSet::new(),
            sources: vec!["knaben".into()],
            magnet: None,
            torrent_url: None,
            info_url: None,
        }
    }

    #[test]
    fn a_unique_prefix_names_a_download_before_a_result_with_the_same_id() {
        let jobs = [(tid("9b5212"), Some("arch"))];
        let results = [release(tid("9b5212"), "arch"), release(tid("51e0c7"), "older arch")];
        let p = "9b52".parse().unwrap();
        assert_eq!(prefix(&p, &jobs, &results, Scope::DownloadsAndResults, "get").unwrap(), Found::Job(tid("9b5212")));
        let p = "51e0".parse().unwrap();
        assert_eq!(
            prefix(&p, &jobs, &results, Scope::DownloadsAndResults, "get").unwrap(),
            Found::Release(&results[1])
        );
        assert_eq!(prefix(&p, &jobs, &results, Scope::Downloads, "pause").unwrap_err().code, ErrorCode::NotFound);
    }

    #[test]
    fn ambiguity_lists_candidates_with_a_longer_prefix() {
        let results =
            [release(tid("e3a1f0"), "ubuntu-24.04.5-desktop-amd64.iso"), release(tid("e3a1c4"), "Ubuntu repack")];
        let err = prefix(&"e3a1".parse().unwrap(), &[], &results, Scope::DownloadsAndResults, "get").unwrap_err();
        assert_eq!(err.code, ErrorCode::AmbiguousRef);
        assert_eq!(err.message, "e3a1 matches 2 torrents");
        assert_eq!(err.hint.as_deref(), Some("use a longer prefix, such as: torrential get e3a1f0"));
        let Some(Details::Candidates(c)) = err.details else { panic!("expected candidates") };
        assert_eq!(
            c.iter().map(|c| (c.id.as_str(), c.kind.as_str())).collect::<Vec<_>>(),
            [("e3a1f0", "search result"), ("e3a1c4", "search result")]
        );
    }
}
