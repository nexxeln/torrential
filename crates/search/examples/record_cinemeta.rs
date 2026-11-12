//! Records what Cinemeta answers for the catalog tests into `tests/fixtures/cinemeta`.
//!
//! `cargo run -p torrential-search --example record_cinemeta`

use std::path::Path;

const BASE: &str = "https://v3-cinemeta.strem.io/";

/// Each recorded answer, by its path under `BASE` and its fixture name.
const ANSWERS: [(&str, &str); 8] = [
    ("catalog/movie/top/search=night%20of%20the%20living%20dead.json", "search-movie-night-of-the-living-dead.json"),
    ("catalog/series/top/search=night%20of%20the%20living%20dead.json", "search-series-night-of-the-living-dead.json"),
    ("meta/movie/tt0063350.json", "meta-movie-tt0063350.json"),
    ("meta/series/tt0063350.json", "meta-series-tt0063350.json"),
    ("meta/movie/tt1727587.json", "meta-movie-tt1727587.json"),
    ("meta/series/tt1727587.json", "meta-series-tt1727587.json"),
    ("meta/series/tt0903747.json", "meta-series-tt0903747.json"),
    // Cinemeta answers a series asked for as a film with another film's name.
    ("meta/movie/tt0903747.json", "meta-movie-tt0903747.json"),
];

const POSTER: (&str, &str) = ("https://images.metahub.space/poster/small/tt1727587/img", "poster-small-tt1727587.jpg");

/// A fixture larger than this is skipped, to be trimmed by hand.
const MAX_BODY: usize = 256 << 10;

#[tokio::main]
async fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cinemeta");
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    let answers = ANSWERS.map(|(path, name)| (format!("{BASE}{path}"), name));
    let client = reqwest::Client::builder().user_agent("Mozilla/5.0").build().expect("an HTTP client");
    for (url, name) in answers.iter().map(|(url, name)| (url.as_str(), *name)).chain([POSTER]) {
        let response = client.get(url).send().await.and_then(reqwest::Response::error_for_status);
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
        std::fs::write(dir.join(name), &body).expect("write the fixture");
        println!("{url} -> cinemeta/{name} ({} bytes)", body.len());
    }
}
