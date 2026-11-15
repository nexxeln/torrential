use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use torrential_search::{SearchConfig, WebSeeds};

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

fn web_seeds(config: SearchConfig) -> WebSeeds {
    WebSeeds::new(&config).unwrap()
}

#[tokio::test]
async fn a_web_seed_that_redirects_a_file_adds_the_folder_it_redirects_to() {
    let base = seed().await;
    let file = Path::new("Big Set/disc 1.mkv");
    let seeds = [format!("{base}/download/"), "/7/items/".to_string()];

    let found = web_seeds(SearchConfig::default()).lookup("Big Set", Some(file), &seeds).unwrap().await.unwrap();

    assert_eq!(found, [format!("{base}/0/items/")]);
}

#[tokio::test]
async fn web_seeds_that_serve_the_file_or_redirect_to_one_file_add_none() {
    let base = seed().await;
    let file = Path::new("Big Set/disc 1.mkv");
    let seeds = [format!("{base}/direct/"), format!("{base}/signed/"), "http://127.0.0.1:1/".to_string()];

    let found = web_seeds(SearchConfig::default()).lookup("Big Set", Some(file), &seeds).unwrap().await.unwrap();

    assert_eq!(found, Vec::<String>::new());
}

/// A server that redirects every request to `to` followed by the path, and counts what it is asked.
async fn redirecting(to: String) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let asked = Arc::new(AtomicUsize::new(0));
    let count = asked.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let (to, count) = (to.clone(), count.clone());
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let path = String::from_utf8_lossy(&head).split_whitespace().nth(1).unwrap_or("/").to_string();
                let reply = if to.is_empty() {
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/9\r\nContent-Length: 1\r\n\r\nx"
                        .to_string()
                } else {
                    format!("HTTP/1.1 302 Found\r\nLocation: {to}{path}\r\nContent-Length: 0\r\n\r\n")
                };
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    (base, asked)
}

#[tokio::test]
async fn a_web_seed_cannot_redirect_the_lookup_to_this_machine_or_the_home_network() {
    let (inside, asked) = redirecting(String::new()).await;
    let port = inside.rsplit(':').next().unwrap();
    let (to_address, _) = redirecting(inside.clone()).await;
    let (to_name, _) = redirecting(format!("http://localhost:{port}")).await;
    let file = Path::new("Big Set/disc 1.mkv");
    let seeds = [format!("{to_address}/set/"), format!("{to_name}/set/")];

    let found = web_seeds(SearchConfig::default()).lookup("Big Set", Some(file), &seeds).unwrap().await.unwrap();

    assert_eq!(found, Vec::<String>::new());
    assert_eq!(asked.load(Ordering::SeqCst), 0, "nothing reached the server inside");
}

#[tokio::test]
async fn a_torrent_with_no_web_seed_to_ask_has_no_lookup() {
    let web_seeds = web_seeds(SearchConfig::default());
    let file = Path::new("Big Set/disc 1.mkv");

    assert!(web_seeds.lookup("Big Set", Some(file), &[]).is_none());
    assert!(web_seeds.lookup("Big Set", Some(file), &["/7/items/".to_string()]).is_none());
    assert!(web_seeds.lookup("one.iso", None, &["http://seed.example/".to_string()]).is_none());
}

#[tokio::test]
async fn archive_org_names_the_servers_of_its_torrent_unless_its_source_is_disabled() {
    let base = seed().await;
    let endpoints = BTreeMap::from([("archive.org".to_string(), format!("{base}/archive/"))]);
    let file = Path::new("sintel/Sintel.mkv");
    let seeds = [format!("{base}/archive/download/")];

    let enabled = web_seeds(SearchConfig { endpoints: endpoints.clone(), ..SearchConfig::default() });
    let named = enabled.lookup("sintel", Some(file), &seeds).unwrap().await.unwrap();
    assert_eq!(named, ["http://ia1.example/3/items/"]);

    let disabled =
        web_seeds(SearchConfig { endpoints, disabled: vec!["archive.org".into()], ..SearchConfig::default() });
    let redirected = disabled.lookup("sintel", Some(file), &seeds).unwrap().await.unwrap();
    assert_eq!(redirected, [format!("{base}/0/items/")]);
}
