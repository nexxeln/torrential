//! Records what Cinemeta and IMDb's suggestions answer for the catalog tests into
//! `tests/fixtures/cinemeta` and `tests/fixtures/imdb`. With arguments, records only the answers
//! whose fixture names hold one of them, so the others keep what the tests expect.
//!
//! `cargo run -p torrential-search --example record_catalog [-- sintel big-buck-bunny]`

use std::path::Path;

const CINEMETA: &str = "https://v3-cinemeta.strem.io/";
const IMDB: &str = "https://v3.sg.media-imdb.com/";

/// Each recorded answer: where it is asked for, and its fixture under `tests/fixtures`.
const ANSWERS: [(&str, &str, &str); 19] = [
    (
        CINEMETA,
        "catalog/movie/top/search=night%20of%20the%20living%20dead.json",
        "cinemeta/search-movie-night-of-the-living-dead.json",
    ),
    (
        CINEMETA,
        "catalog/series/top/search=night%20of%20the%20living%20dead.json",
        "cinemeta/search-series-night-of-the-living-dead.json",
    ),
    (CINEMETA, "catalog/movie/top/search=sintel.json", "cinemeta/search-movie-sintel.json"),
    (CINEMETA, "catalog/series/top/search=sintel.json", "cinemeta/search-series-sintel.json"),
    (CINEMETA, "catalog/movie/top/search=big%20buck%20bunny.json", "cinemeta/search-movie-big-buck-bunny.json"),
    (CINEMETA, "catalog/series/top/search=big%20buck%20bunny.json", "cinemeta/search-series-big-buck-bunny.json"),
    (CINEMETA, "meta/movie/tt0063350.json", "cinemeta/meta-movie-tt0063350.json"),
    (CINEMETA, "meta/series/tt0063350.json", "cinemeta/meta-series-tt0063350.json"),
    (CINEMETA, "meta/movie/tt1727587.json", "cinemeta/meta-movie-tt1727587.json"),
    (CINEMETA, "meta/series/tt1727587.json", "cinemeta/meta-series-tt1727587.json"),
    (CINEMETA, "meta/series/tt0903747.json", "cinemeta/meta-series-tt0903747.json"),
    // Cinemeta answers a series asked for as a film with another film's name.
    (CINEMETA, "meta/movie/tt0903747.json", "cinemeta/meta-movie-tt0903747.json"),
    (IMDB, "suggestion/x/sintel.json", "imdb/suggestion-sintel.json"),
    (IMDB, "suggestion/x/breaking%20bad.json", "imdb/suggestion-breaking-bad.json"),
    (IMDB, "suggestion/x/big%20buck%20bunny.json", "imdb/suggestion-big-buck-bunny.json"),
    (IMDB, "suggestion/x/night%20of%20the%20living%20dead.json", "imdb/suggestion-night-of-the-living-dead.json"),
    // Lists a video to leave out and a TV film to keep.
    (IMDB, "suggestion/x/the%20matrix.json", "imdb/suggestion-the-matrix.json"),
    (IMDB, "suggestion/x/the%20office.json", "imdb/suggestion-the-office.json"),
    ("https://images.metahub.space/", "poster/small/tt1727587/img", "cinemeta/poster-small-tt1727587.jpg"),
];

/// A fixture larger than this is skipped, to be trimmed by hand.
const MAX_BODY: usize = 256 << 10;

#[tokio::main]
async fn main() {
    let only: Vec<String> = std::env::args().skip(1).collect();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let client = reqwest::Client::builder().user_agent("Mozilla/5.0").build().expect("an HTTP client");
    let wanted =
        ANSWERS.iter().filter(|(_, _, name)| only.is_empty() || only.iter().any(|o| name.contains(o.as_str())));
    for (base, path, name) in wanted {
        let url = format!("{base}{path}");
        let response = client.get(&url).send().await.and_then(reqwest::Response::error_for_status);
        let body = match response {
            Ok(response) => response.bytes().await.expect("read the answer"),
            Err(e) => {
                eprintln!("{url}: {e}");
                std::process::exit(1);
            }
        };
        if body.len() > MAX_BODY {
            println!("skipped {url} ({} bytes)", body.len());
            continue;
        }
        let file = dir.join(name);
        std::fs::create_dir_all(file.parent().expect("a fixture has a folder")).expect("create the fixture folder");
        std::fs::write(file, &body).expect("write the fixture");
        println!("{url} -> {name} ({} bytes)", body.len());
    }
}
