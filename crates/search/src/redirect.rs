//! Where a torrent's web seeds send the request for one of its files.
//!
//! A web seed can answer each file with a redirect to a server that holds it. libtorrent turns
//! each redirect into a web seed that has only that file, so a piece that spans two files has no
//! source. A redirect that keeps the file's path names a folder that serves every file, and
//! libtorrent downloads from that folder as from any web seed.

use std::path::Path;
use std::time::Duration;

use futures_util::future::join_all;
use percent_encoding::percent_decode_str;
use tokio::time::Instant;
use url::Url;

use crate::http::Http;

/// The URL libtorrent asks the web seed `seed` for `file`, a path that starts with the torrent's
/// name.
pub(crate) fn request(seed: &str, file: &Path) -> Option<Url> {
    let mut url = Url::parse(seed).ok().filter(|u| matches!(u.scheme(), "http" | "https"))?;
    let names: Vec<&str> = file.iter().map(|name| name.to_str()).collect::<Option<_>>()?;
    url.path_segments_mut().ok()?.pop_if_empty().extend(names);
    Some(url)
}

/// The web seed that serves the torrent from where the request `asked` for `file` landed. `None`
/// when no redirect moved the request, and when the redirect leads to that one file only, such as
/// a signed link or a renamed copy.
pub(crate) fn root(file: &Path, asked: &Url, landed: &Url) -> Option<String> {
    if landed == asked || landed.query().is_some() {
        return None;
    }
    let names: Vec<&str> = file.iter().map(|name| name.to_str()).collect::<Option<_>>()?;
    let segments: Vec<&str> = landed.path_segments()?.collect();
    let folder = segments.len().checked_sub(names.len()).filter(|_| !names.is_empty())?;
    let kept = |(segment, name): (&&str, &&str)| percent_decode_str(segment).decode_utf8().is_ok_and(|s| s == **name);
    if !segments[folder..].iter().zip(&names).all(kept) {
        return None;
    }
    let mut root = landed.clone();
    root.set_fragment(None);
    {
        let mut folder = root.path_segments_mut().ok()?;
        for _ in &names {
            folder.pop();
        }
        folder.push("");
    }
    Some(root.into())
}

/// The web seeds that serve the torrent from where its own `web_seeds` redirect `file`, in their
/// order. A seed that does not answer adds none.
pub(crate) async fn roots(http: &Http, file: &Path, web_seeds: &[String]) -> Vec<String> {
    const WITHIN: Duration = Duration::from_secs(20);
    let deadline = Instant::now() + WITHIN;
    let landings = web_seeds.iter().filter_map(|seed| request(seed, file)).map(|asked| async move {
        let landed = http.landing(&asked, deadline).await.ok()?;
        root(file, &asked, &landed)
    });
    let mut roots = Vec::new();
    for root in join_all(landings).await.into_iter().flatten() {
        if !roots.contains(&root) && !web_seeds.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn asks_a_web_seed_for_a_file_the_way_libtorrent_does() {
        let file = Path::new("sintel.-2010.1080p/Sintel.2010.1080p.mkv");
        assert_eq!(
            request("https://archive.org/download/", file).unwrap().as_str(),
            "https://archive.org/download/sintel.-2010.1080p/Sintel.2010.1080p.mkv"
        );
        assert_eq!(
            request("http://seed.example/files", Path::new("Big Set/disc 1/a#1.iso")).unwrap().as_str(),
            "http://seed.example/files/Big%20Set/disc%201/a%231.iso"
        );
        assert_eq!(request("/7/items/", file), None);
        assert_eq!(request("ftp://seed.example/", file), None);
    }

    #[test]
    fn a_redirect_that_keeps_the_files_path_names_a_folder_that_serves_every_file() {
        let file = Path::new("sintel.-2010.1080p/Sintel.2010.1080p.mkv");
        let asked = url("https://archive.org/download/sintel.-2010.1080p/Sintel.2010.1080p.mkv");
        let landed = url("https://dn721609.ca.archive.org/0/items/sintel.-2010.1080p/Sintel.2010.1080p.mkv");
        assert_eq!(root(file, &asked, &landed).unwrap(), "https://dn721609.ca.archive.org/0/items/");

        let spaced = Path::new("Big Set/a b.iso");
        let asked = url("http://seed.example/Big%20Set/a%20b.iso");
        assert_eq!(
            root(spaced, &asked, &url("http://cdn.example:8080/Big%20Set/a%20b.iso")).unwrap(),
            "http://cdn.example:8080/"
        );
    }

    #[test]
    fn a_seed_that_serves_the_file_itself_or_redirects_to_one_file_adds_no_folder() {
        let file = Path::new("set/a.mkv");
        let asked = url("https://seed.example/set/a.mkv");
        assert_eq!(root(file, &asked, &asked), None);
        assert_eq!(root(file, &asked, &url("https://cdn.example/set/a.mkv?signature=abc")), None);
        assert_eq!(root(file, &asked, &url("https://cdn.example/blobs/1f3a")), None);
        assert_eq!(root(file, &asked, &url("https://cdn.example/other/a.mkv")), None);
    }
}
