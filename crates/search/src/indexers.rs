//! Asks an address what it is before torrential searches it: a Torznab endpoint, or a Prowlarr
//! or Jackett server, whose indexers each have an endpoint of their own.

use std::time::Duration;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::Deserialize;
use tokio::time::Instant;
use torrential_core::indexer::{ApiKey, Check, IndexerCategory};
use torrential_core::{Error, ErrorCode, text};
use url::Url;

use crate::http::{FetchError, Http, Retry};

/// How long one question to an indexer may take.
const WITHIN: Duration = Duration::from_secs(10);
const CAP: usize = 4 << 20;

/// What an address turned out to be.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Found {
    /// A Torznab endpoint, which answered `t=caps` with these categories.
    Endpoint { categories: Vec<IndexerCategory> },
    /// A Prowlarr or Jackett server and each indexer it has.
    Server { app: App, indexers: Vec<Hosted> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum App {
    Prowlarr,
    Jackett,
}

impl App {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prowlarr => "Prowlarr",
            Self::Jackett => "Jackett",
        }
    }
}

/// An indexer on a Prowlarr or Jackett server: its name there, and its Torznab endpoint, which
/// takes the server's key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hosted {
    pub name: String,
    pub url: Url,
}

/// Why an address is not an indexer torrential can search.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Problem {
    /// No answer, and why.
    Unreachable(String),
    /// It asks for a key, or refused the one it was given.
    Key,
    /// It answered, but not as a Torznab indexer: what it said.
    NotTorznab(String),
}

impl Problem {
    fn reason(&self, keyed: bool) -> String {
        match self {
            Self::Unreachable(why) => format!("cannot reach it: {why}"),
            Self::Key if keyed => "it refused the API key".into(),
            Self::Key => "it needs an API key".into(),
            Self::NotTorznab(what) => what.clone(),
        }
    }

    fn error(self, url: &Url, keyed: bool) -> Error {
        let said = self.reason(keyed);
        match self {
            Self::Unreachable(_) => Error::new(ErrorCode::FetchFailed, format!("{url}: {said}"))
                .with_hint("check that the indexer runs, and that the engine's machine reaches this address"),
            Self::Key => Error::new(ErrorCode::Usage, format!("{url}: {said}"))
                .with_hint("copy the API key from the settings of Jackett or Prowlarr, and pass it with --key"),
            Self::NotTorznab(_) => {
                Error::new(ErrorCode::Usage, format!("{url} does not answer as a Torznab indexer: {said}"))
                    .with_hint("pass an indexer's Torznab feed URL, or the address of Prowlarr or Jackett itself")
            }
        }
    }
}

/// Asks indexers what they are and whether they answer.
pub struct Probe {
    http: Http,
}

impl Probe {
    pub fn new() -> Result<Probe, Error> {
        let http = Http::new().map_err(|e| Error::internal(format!("cannot build the HTTP client: {e}")))?;
        Ok(Probe { http })
    }

    /// What `url` is: a Torznab endpoint, else a Prowlarr or Jackett server with at least one
    /// indexer. Fails with `fetch_failed` when nothing answers there, and with `usage` when its
    /// answer refuses the key or is neither.
    pub async fn find(&self, url: &Url, key: Option<&ApiKey>) -> Result<Found, Error> {
        let keyed = key.is_some();
        let first = match self.caps(url, key).await {
            Ok(categories) => return Ok(Found::Endpoint { categories }),
            Err(problem @ Problem::NotTorznab(_)) => problem,
            Err(problem) => return Err(problem.error(url, keyed)),
        };
        for app in [App::Prowlarr, App::Jackett] {
            let indexers = match self.hosted(app, url, key).await {
                Ok(Some(indexers)) => indexers,
                Ok(None) => continue,
                Err(problem) => return Err(problem.error(url, keyed)),
            };
            if indexers.is_empty() {
                let what = if app == App::Prowlarr { "enabled torrent indexers" } else { "configured indexers" };
                return Err(Error::new(ErrorCode::Usage, format!("{} at {url} has no {what}", app.as_str()))
                    .with_hint(format!("add your tracker in {}, then run this again", app.as_str())));
            }
            return Ok(Found::Server { app, indexers });
        }
        Err(first.error(url, keyed))
    }

    /// Whether the Torznab endpoint at `url` answers now, and with which categories.
    pub async fn check(&self, url: &Url, key: Option<&ApiKey>) -> Check {
        match self.caps(url, key).await {
            Ok(categories) => Check::Answers { categories },
            Err(problem) => Check::Fails { reason: problem.reason(key.is_some()) },
        }
    }

    async fn caps(&self, url: &Url, key: Option<&ApiKey>) -> Result<Vec<IndexerCategory>, Problem> {
        let body = self.get(&with_query(url, &[("t", "caps")], key)).await?;
        caps(&body)
    }

    /// The indexers of the `app` server at `base`. `None` when `base` is not such a server.
    async fn hosted(&self, app: App, base: &Url, key: Option<&ApiKey>) -> Result<Option<Vec<Hosted>>, Problem> {
        let not_it = |problem: Problem| match problem {
            Problem::NotTorznab(_) => Ok(None),
            other => Err(other),
        };
        match app {
            App::Prowlarr => {
                let status = match self.get(&with_query(&under(base, "api/v1/system/status"), &[], key)).await {
                    Ok(body) => body,
                    Err(problem) => return not_it(problem),
                };
                if serde_json::from_slice::<Status>(&status).ok().is_none_or(|s| s.app_name != "Prowlarr") {
                    return Ok(None);
                }
                let listed = self.get(&with_query(&under(base, "api/v1/indexer"), &[], key)).await?;
                let listed: Vec<Listed> = serde_json::from_slice(&listed)
                    .map_err(|e| Problem::NotTorznab(format!("its list of indexers did not read: {e}")))?;
                let enabled = listed.into_iter().filter(|i| i.enable && i.protocol == "torrent");
                Ok(Some(enabled.map(|i| Hosted { name: i.name, url: under(base, &format!("{}/api", i.id)) }).collect()))
            }
            App::Jackett => {
                let all = under(base, "api/v2.0/indexers/all/results/torznab/api");
                let body = match self.get(&with_query(&all, &[("t", "indexers"), ("configured", "true")], key)).await {
                    Ok(body) => body,
                    Err(problem) => return not_it(problem),
                };
                let Some(ids) = jackett_indexers(&body)? else { return Ok(None) };
                let endpoint = |id: &str| under(base, &format!("api/v2.0/indexers/{id}/results/torznab/api"));
                Ok(Some(ids.into_iter().map(|id| Hosted { url: endpoint(&id), name: id }).collect()))
            }
        }
    }

    async fn get(&self, url: &Url) -> Result<Vec<u8>, Problem> {
        self.http.fetch(url, CAP, Instant::now() + WITHIN, Retry::Never).await.map_err(|e| match e {
            FetchError::Status(401 | 403) => Problem::Key,
            FetchError::Status(status) => Problem::NotTorznab(format!("it answered HTTP {status}")),
            FetchError::Timeout(_) => Problem::Unreachable(format!("no answer in {} seconds", WITHIN.as_secs())),
            FetchError::Connect(why) | FetchError::Decode(why) => Problem::Unreachable(clean(&why)),
            FetchError::TooLarge(cap) => Problem::NotTorznab(format!("its answer is larger than {} MiB", cap >> 20)),
        })
    }
}

/// `url` with `pairs` and the key added to its query.
fn with_query(url: &Url, pairs: &[(&str, &str)], key: Option<&ApiKey>) -> Url {
    let mut url = url.clone();
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs(pairs);
        if let Some(key) = key {
            query.append_pair("apikey", key.expose());
        }
    }
    url
}

/// `path` under `base`, which may itself be under a path, such as `http://nas/prowlarr`.
fn under(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_query(None);
    url.set_fragment(None);
    url.set_path(&format!("{}/{path}", base.path().trim_end_matches('/')));
    url
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    app_name: String,
}

#[derive(Deserialize)]
struct Listed {
    id: u32,
    name: String,
    enable: bool,
    protocol: String,
}

/// Text an indexer sent, safe to print.
fn clean(text: &str) -> String {
    text::truncate(&text::sanitize(text), 160)
}

/// The standard top-level categories of a `t=caps` answer, each once. An indexer's own (100000
/// and up) are left out: Jackett and Prowlarr list the standard one each maps to as well.
fn caps(body: &[u8]) -> Result<Vec<IndexerCategory>, Problem> {
    let mut categories: Vec<IndexerCategory> = Vec::new();
    for_elements(body, "caps", |e| {
        if e.depth == 2 && e.name == "category" {
            let id = e.attr("id").and_then(|id| id.trim().parse().ok()).filter(|id| *id < 100_000);
            if let (Some(id), Some(name)) = (id, e.attr("name"))
                && !categories.iter().any(|c| c.id == id)
            {
                categories.push(IndexerCategory { id, name: clean(&name) });
            }
        }
    })?;
    Ok(categories)
}

/// The IDs of the indexers a Jackett `t=indexers` answer lists. `None` when the answer is not one.
fn jackett_indexers(body: &[u8]) -> Result<Option<Vec<String>>, Problem> {
    let mut ids = Vec::new();
    let read = for_elements(body, "indexers", |e| {
        if e.depth == 1 && e.name == "indexer" && e.attr("configured").is_none_or(|c| c == "true") {
            ids.extend(e.attr("id").filter(|id| valid_id(id)));
        }
    });
    match read {
        Ok(()) => Ok(Some(ids)),
        Err(Problem::NotTorznab(_)) => Ok(None),
        Err(problem) => Err(problem),
    }
}

/// A Jackett indexer ID that stays one path segment.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) && id != "." && id != ".."
}

struct Element<'a> {
    /// 0 for the root.
    depth: usize,
    name: String,
    start: &'a BytesStart<'a>,
}

impl Element<'_> {
    fn attr(&self, name: &str) -> Option<String> {
        let found = self.start.attributes().flatten().find(|a| a.key.local_name().as_ref() == name)?;
        found.normalized_value(XmlVersion::default()).ok().map(|v| v.into_owned())
    }
}

/// Calls `each` on every element of an XML answer whose root is `root`. A Torznab `<error>` root
/// says why it is not, and anything else is not Torznab.
fn for_elements(body: &[u8], root: &str, mut each: impl FnMut(&Element)) -> Result<(), Problem> {
    let text = String::from_utf8_lossy(body);
    let mut reader = Reader::from_str(&text);
    let mut depth = 0;
    loop {
        let event = reader.read_event().map_err(|_| not_torznab(&text))?;
        match event {
            Event::Start(ref start) | Event::Empty(ref start) => {
                let name = start.local_name().as_ref().to_ascii_lowercase();
                let element = Element { depth, name, start };
                if depth == 0 && element.name != root {
                    return Err(match element.name.as_str() {
                        "error" => torznab_error(&element),
                        _ => not_torznab(&text),
                    });
                }
                each(&element);
                if matches!(event, Event::Start(_)) {
                    depth += 1;
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            Event::Eof if depth == 0 => return Err(not_torznab(&text)),
            Event::Eof => return Ok(()),
            _ => {}
        }
        if depth == 0 && matches!(event, Event::End(_) | Event::Empty(_)) {
            return Ok(());
        }
    }
}

/// Torznab's error codes 100 to 199 are about the key or the account it belongs to.
fn torznab_error(element: &Element) -> Problem {
    let code: u32 = element.attr("code").and_then(|code| code.trim().parse().ok()).unwrap_or(0);
    if (100..200).contains(&code) {
        return Problem::Key;
    }
    let description = element.attr("description").map(|d| clean(&d)).unwrap_or_default();
    Problem::NotTorznab(format!("it answered error {code}: {description}"))
}

fn not_torznab(text: &str) -> Problem {
    let page =
        text.trim_start().get(..15).is_some_and(|start| start.to_ascii_lowercase().starts_with("<!doctype html"))
            || text.contains("<html");
    let what = if page { "it answered a web page" } else { "it answered something else" };
    Problem::NotTorznab(format!("{what}, not Torznab"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server title="Jackett" />
  <searching><search available="yes" supportedParams="q" /></searching>
  <categories>
    <category id="2000" name="Movies">
      <subcat id="2040" name="Movies/HD" />
    </category>
    <category id="5000" name="TV &amp; Series" />
    <category id="5000" name="TV" />
    <category id="100208" name="HD - TV shows" />
  </categories>
</caps>"#;

    #[test]
    fn caps_lists_the_standard_top_level_categories_once() {
        let categories = caps(CAPS.as_bytes()).unwrap();
        let named: Vec<(u32, &str)> = categories.iter().map(|c| (c.id, c.name.as_str())).collect();
        assert_eq!(named, [(2000, "Movies"), (5000, "TV & Series")]);
        assert_eq!(caps(b"<caps><categories /></caps>"), Ok(vec![]));
    }

    #[test]
    fn an_answer_that_is_not_caps_says_what_it_was() {
        let cases: [(&str, Problem); 5] = [
            (r#"<?xml version="1.0"?><error code="100" description="Invalid API Key" />"#, Problem::Key),
            (r#"<error code="102" description="Insufficient privileges" />"#, Problem::Key),
            (
                r#"<error code="201" description="Indexer is not configured" />"#,
                Problem::NotTorznab("it answered error 201: Indexer is not configured".into()),
            ),
            (
                "<!doctype html><html lang=\"en\"><head></head></html>",
                Problem::NotTorznab("it answered a web page, not Torznab".into()),
            ),
            (r#"{"appName": "Sonarr"}"#, Problem::NotTorznab("it answered something else, not Torznab".into())),
        ];
        for (body, expected) in cases {
            assert_eq!(caps(body.as_bytes()), Err(expected), "{body}");
        }
    }

    #[test]
    fn jackett_lists_its_configured_indexers_by_id() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<indexers>
  <indexer id="thepiratebay" configured="true"><title>The Pirate Bay</title>
    <caps><categories><category id="2000" name="Movies" /></categories></caps>
  </indexer>
  <indexer id="filelist" configured="true"><title>FileList</title></indexer>
  <indexer id="../admin" configured="true"><title>Bad</title></indexer>
</indexers>"#;
        assert_eq!(jackett_indexers(body.as_bytes()), Ok(Some(vec!["thepiratebay".into(), "filelist".into()])));
        assert_eq!(jackett_indexers(b"<indexers />"), Ok(Some(vec![])));
        assert_eq!(jackett_indexers(b"<html></html>"), Ok(None));
        assert_eq!(jackett_indexers(br#"<error code="100" description="Invalid API Key" />"#), Err(Problem::Key));
    }

    #[test]
    fn paths_go_under_a_base_with_or_without_its_own_path() {
        let base = Url::parse("http://nas.local/prowlarr/?apikey=k3y").unwrap();
        assert_eq!(under(&base, "1/api").as_str(), "http://nas.local/prowlarr/1/api");
        let bare = Url::parse("http://127.0.0.1:9117").unwrap();
        assert_eq!(
            under(&bare, "api/v2.0/indexers/x/results/torznab/api").as_str(),
            "http://127.0.0.1:9117/api/v2.0/indexers/x/results/torznab/api"
        );
    }

    #[test]
    fn errors_name_the_address_and_never_the_key() {
        let url = Url::parse("http://127.0.0.1:9696/3/api").unwrap();
        let key = Problem::Key.error(&url, true);
        assert_eq!(
            (key.code, key.message.as_str()),
            (ErrorCode::Usage, "http://127.0.0.1:9696/3/api: it refused the API key")
        );
        let none = Problem::Key.error(&url, false);
        assert_eq!(none.message, "http://127.0.0.1:9696/3/api: it needs an API key");
        let asked = with_query(&url, &[("t", "caps")], ApiKey::new("k3y").as_ref());
        assert_eq!(asked.as_str(), "http://127.0.0.1:9696/3/api?t=caps&apikey=k3y");
    }
}
