//! The built web app, which build.rs embeds in the binary.

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

const INDEX: &str = "/index.html";
/// Vite names these files by a hash of their content, so a browser can keep them for good.
const HASHED: &str = "/assets/";

const TYPES: [(&str, &str); 14] = [
    ("html", "text/html; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("woff2", "font/woff2"),
    ("woff", "font/woff"),
    ("txt", "text/plain; charset=utf-8"),
    ("webmanifest", "application/manifest+json"),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Asset {
    pub bytes: &'static [u8],
    pub content_type: &'static str,
    /// The `Cache-Control` value.
    pub cache: &'static str,
}

/// The file at `path`. A path that names no file is a page of the app, which index.html renders.
pub fn find(path: &str) -> Option<Asset> {
    find_in(FILES, path)
}

fn find_in(files: &'static [(&'static str, &'static [u8])], path: &str) -> Option<Asset> {
    let file = |path: &str| files.iter().find(|(route, _)| *route == path).copied();
    let name = path.rsplit('/').next().unwrap_or_default();
    let (route, bytes) = match file(path) {
        Some(found) => found,
        None if path.starts_with(HASHED) || name.contains('.') => return None,
        None => file(INDEX)?,
    };
    let extension = route.rsplit_once('.').map_or("", |(_, extension)| extension);
    let content_type = TYPES.iter().find(|(e, _)| *e == extension).map_or("application/octet-stream", |(_, t)| t);
    let cache = if route.starts_with(HASHED) { "public, max-age=31536000, immutable" } else { "no-cache" };
    Some(Asset { bytes, content_type, cache })
}

#[cfg(test)]
mod tests {
    use super::*;

    static APP: &[(&str, &[u8])] =
        &[("/assets/index-a1b2.js", b"js"), ("/favicon.svg", b"svg"), ("/index.html", b"page")];

    #[test]
    fn a_path_is_its_file_or_a_page_of_the_app() {
        let read = |path| find_in(APP, path).map(|asset| (asset.bytes, asset.content_type, asset.cache));
        let page = Some((&b"page"[..], "text/html; charset=utf-8", "no-cache"));
        assert_eq!(read("/"), page);
        assert_eq!(read("/watch/9b5212"), page);
        assert_eq!(read("/downloads"), page);
        assert_eq!(read("/index.html"), page);
        assert_eq!(
            read("/assets/index-a1b2.js"),
            Some((&b"js"[..], "text/javascript; charset=utf-8", "public, max-age=31536000, immutable"))
        );
        assert_eq!(read("/favicon.svg"), Some((&b"svg"[..], "image/svg+xml", "no-cache")));
        assert_eq!(read("/assets/index-old.js"), None, "a stale script must not get the page");
        assert_eq!(read("/robots.txt"), None);
    }

    #[test]
    fn the_binary_carries_the_app() {
        assert!(find("/").is_some_and(|page| page.content_type.starts_with("text/html")));
    }
}
