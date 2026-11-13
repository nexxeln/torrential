use std::collections::BTreeMap;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrential_search::{ResultCache, Search, SearchConfig};

/// A server that redirects a path under `download/` to `/0/items/`, redirects `/signed/...` to one
/// signed file, names the servers of an archive.org item, and answers every other path with one
/// byte.
async fn seed() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string();
                    head.clear();
                    let reply = if let Some((_, file)) = path.split_once("/download/") {
                        format!("HTTP/1.1 302 Found\r\nLocation: /0/items/{file}\r\nContent-Length: 0\r\n\r\n")
                    } else if path.contains("/metadata/") {
                        let body = r#"{"d1":"ia1.example","dir":"/3/items/sintel"}"#;
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                    } else if path.starts_with("/signed/") {
                        "HTTP/1.1 302 Found\r\nLocation: /blob/1f3a?signature=abc\r\nContent-Length: 0\r\n\r\n"
                            .to_string()
                    } else {
                        "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/9\r\nContent-Length: 1\r\n\r\nx"
                            .to_string()
                    };
                    if socket.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    base
}

fn search(config: SearchConfig) -> Search {
    Search::new(&config, ResultCache::new()).unwrap()
}

#[tokio::test]
async fn a_web_seed_that_redirects_a_file_adds_the_folder_it_redirects_to() {
    let base = seed().await;
    let file = Path::new("Big Set/disc 1.mkv");
    let seeds = [format!("{base}/download/"), "/7/items/".to_string()];

    let found = search(SearchConfig::default()).mirrors("Big Set", Some(file), &seeds).unwrap().await.unwrap();

    assert_eq!(found, [format!("{base}/0/items/")]);
}

#[tokio::test]
async fn web_seeds_that_serve_the_file_or_redirect_to_one_file_add_none() {
    let base = seed().await;
    let file = Path::new("Big Set/disc 1.mkv");
    let seeds = [format!("{base}/direct/"), format!("{base}/signed/"), "http://127.0.0.1:1/".to_string()];

    let found = search(SearchConfig::default()).mirrors("Big Set", Some(file), &seeds).unwrap().await.unwrap();

    assert_eq!(found, Vec::<String>::new());
}

#[tokio::test]
async fn a_torrent_with_no_web_seed_to_ask_has_no_lookup() {
    let search = search(SearchConfig::default());
    let file = Path::new("Big Set/disc 1.mkv");

    assert!(search.mirrors("Big Set", Some(file), &[]).is_none());
    assert!(search.mirrors("Big Set", Some(file), &["/7/items/".to_string()]).is_none());
    assert!(search.mirrors("one.iso", None, &["http://seed.example/".to_string()]).is_none());
}

#[tokio::test]
async fn archive_org_names_the_servers_of_its_torrent_unless_its_source_is_disabled() {
    let base = seed().await;
    let endpoints = BTreeMap::from([("archive.org".to_string(), format!("{base}/archive/"))]);
    let file = Path::new("sintel/Sintel.mkv");
    let seeds = [format!("{base}/archive/download/")];

    let enabled = search(SearchConfig { endpoints: endpoints.clone(), ..SearchConfig::default() });
    let named = enabled.mirrors("sintel", Some(file), &seeds).unwrap().await.unwrap();
    assert_eq!(named, ["http://ia1.example/3/items/"]);

    let disabled = search(SearchConfig { endpoints, disabled: vec!["archive.org".into()], ..SearchConfig::default() });
    let redirected = disabled.mirrors("sintel", Some(file), &seeds).unwrap().await.unwrap();
    assert_eq!(redirected, [format!("{base}/0/items/")]);
}
