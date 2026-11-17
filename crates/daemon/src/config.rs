use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_saphyr::{Localizer, Location, SnippetMode, Spanned, UserMessageFormatter};
use torrential_core::indexer::{ApiKey, KeyFrom};
use torrential_core::units::{parse_size, size_text};
use torrential_core::{Error, ErrorCode, Playback, Preferences, Screen};
use torrential_engine::InterfaceName;
use torrential_search::catalog::CatalogConfig;
use torrential_search::subtitles::SubtitleConfig;
use torrential_search::{SearchConfig, TorznabConfig};

use crate::env::{Env, hooks};
use crate::fsutil::{Writes, write_atomic, write_private};
use crate::paths::Paths;
use crate::player::Choice;

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Config {
    pub downloads: Option<PathBuf>,
    pub search: SearchConfig,
    pub file: Option<PathBuf>,
    pub preferences: Preferences,
    pub updates: Policy,
    pub player: Option<Choice>,
    pub subtitles: SubtitleConfig,
    pub catalog: CatalogConfig,
    pub ffmpeg: FfmpegConfig,
    pub plays_max: PlaysMax,
    pub exposure: Exposure,
    /// The network interface, such as a VPN's `wg0`, that every torrent socket is bound to. The web
    /// app, pairing and casting stay on every network.
    pub torrent_interface: Option<InterfaceName>,
}

/// How devices outside the home reach the engine.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Exposure {
    /// `server: true`: the engine listens on the network with nothing paired, and lets a paired
    /// device in by whatever address it uses.
    pub server: bool,
    /// `public_address`, which pairing links and QR codes use.
    pub public_address: Option<PublicAddress>,
}

impl Exposure {
    /// Whether a request that names `host` came through the HTTPS public address, so the device
    /// cookie it gets must say `Secure`.
    pub fn secure(&self, host: Option<&str>) -> bool {
        self.public_address.as_ref().is_some_and(|public| public.https && host == Some(public.host.as_str()))
    }
}

/// Where devices reach the engine from outside the home, such as `https://tv.example.com` with a
/// reverse proxy in front, or `http://box.tailnet.ts.net:47919`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PublicAddress {
    /// The scheme, host and any port, with no slash after them.
    pub origin: String,
    /// What a browser sends as `Host` there, lowercased.
    pub host: String,
    pub https: bool,
}

impl PublicAddress {
    /// An HTTP or HTTPS origin, with no path, query or user name.
    pub fn parse(text: &str) -> Option<Self> {
        let url = url::Url::parse(text.trim()).ok()?;
        let https = match url.scheme() {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        let bare = url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none();
        let name = url.host_str().filter(|_| bare)?;
        let host = url.port().map_or_else(|| name.to_string(), |port| format!("{name}:{port}"));
        Some(Self { origin: format!("{}://{host}", url.scheme()), host, https })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FfmpegConfig {
    /// The engine fetches a pinned ffmpeg when none is installed and a file needs converting.
    pub fetch: bool,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self { fetch: true }
    }
}

/// The most torrential's plays folder holds before the plays watched longest ago leave it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaysMax(pub u64);

impl Default for PlaysMax {
    fn default() -> Self {
        Self(20 << 30)
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct File {
    downloads: Downloads,
    sources: Sources,
    preferences: Option<Overrides>,
    updates: Option<Policy>,
    player: Option<Spanned<String>>,
    subtitles: Subtitles,
    catalog: Catalog,
    ffmpeg: Ffmpeg,
    plays: Plays,
    server: Option<bool>,
    torrent_interface: Option<Spanned<String>>,
    public_address: Option<Spanned<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Plays {
    /// A size, or none to keep every play.
    max: Option<Spanned<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PreferencesOnly {
    preferences: Option<Overrides>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UpdatesOnly {
    updates: Option<Policy>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServerOnly {
    server: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SourcesOnly {
    sources: Sources,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Overrides {
    screen: Option<Screen>,
    max_size: Option<Spanned<String>>,
    playback: Option<Playback>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Subtitles {
    /// False stops the web player from looking for subtitles online.
    online: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Catalog {
    /// False keeps title lookups and their pictures off the network.
    online: Option<bool>,
    /// A Cinemeta mirror.
    endpoint: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Ffmpeg {
    /// False stops the engine from fetching ffmpeg when none is installed.
    fetch: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Downloads {
    dir: Option<Spanned<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Sources {
    disabled: Vec<String>,
    torznab: Vec<Torznab>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Torznab {
    name: String,
    url: String,
    api_key: Option<Spanned<String>>,
    api_key_env: Option<Spanned<String>>,
}

impl Torznab {
    fn entry(self) -> Result<IndexerEntry, Problem> {
        let Torznab { name, url, api_key, api_key_env } = self;
        let key = match (api_key, api_key_env) {
            (None, None) => Key::None,
            (Some(Spanned { value, referenced, .. }), None) => Key::Inline(
                ApiKey::new(&value)
                    .ok_or_else(|| (Some(referenced.line()), format!("the {name:?} indexer's api_key is empty")))?,
            ),
            (None, Some(Spanned { value, .. })) => Key::Env(value),
            (Some(Spanned { referenced, .. }), Some(_)) => {
                let message = format!("the {name:?} indexer has both api_key and api_key_env; keep one");
                return Err((Some(referenced.line()), message));
            }
        };
        Ok(IndexerEntry { name, url, key })
    }
}

/// A `sources.torznab` entry as config.yaml holds it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IndexerEntry {
    pub name: String,
    pub url: String,
    pub key: Key,
}

/// Where an indexer's API key is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Key {
    None,
    /// `api_key`, in config.yaml itself.
    Inline(ApiKey),
    /// `api_key_env`: the engine's environment variable that holds it.
    Env(String),
}

impl Key {
    /// The key itself, or `None` when there is none or its variable is not set in `env`.
    pub fn resolve(&self, env: &Env) -> Option<ApiKey> {
        match self {
            Self::None => None,
            Self::Inline(key) => Some(key.clone()),
            Self::Env(var) => env.var(var).and_then(ApiKey::new),
        }
    }

    pub fn origin(&self) -> KeyFrom {
        match self {
            Self::None => KeyFrom::None,
            Self::Inline(_) => KeyFrom::Config,
            Self::Env(var) => KeyFrom::Env(var.clone()),
        }
    }
}

/// The Torznab indexers in config.yaml, and the sources it disables.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Indexers {
    pub entries: Vec<IndexerEntry>,
    pub disabled: Vec<String>,
}

type Problem = (Option<u64>, String);

pub fn path(paths: &Paths) -> PathBuf {
    paths.config.join("config.yaml")
}

impl Config {
    pub fn load(paths: &Paths, env: &Env) -> Result<Config, Error> {
        let mut config = match read(paths)? {
            None => Config::default(),
            Some((path, text)) => {
                let config = parse(&text, env).map_err(|(line, message)| invalid(&path, line, &message))?;
                Config { file: Some(path), ..config }
            }
        };
        if let Some(test) = env.test() {
            config.search.endpoints = test.endpoints(hooks::ENDPOINTS)?;
            config.subtitles.endpoints = test.endpoints(hooks::SUBTITLE_ENDPOINTS)?;
            if let Some(endpoint) = test.var(hooks::CATALOG_ENDPOINT) {
                config.catalog.endpoint = Some(endpoint.to_string());
            }
        }
        Ok(config)
    }

    pub fn downloads_dir(&self, paths: &Paths) -> PathBuf {
        self.downloads.clone().unwrap_or_else(|| paths.downloads.clone())
    }
}

/// The download folder `settings` names, or the default one when they name none or did not load.
pub fn downloads_dir(settings: Result<&Config, &Error>, paths: &Paths) -> PathBuf {
    settings.map_or_else(|_| paths.downloads.clone(), |settings| settings.downloads_dir(paths))
}

pub fn file(paths: &Paths) -> Result<PathBuf, Error> {
    Ok(read(paths)?.map_or_else(|| path(paths), |(path, _)| path))
}

pub fn preferences(paths: &Paths) -> Result<Preferences, Error> {
    let Some((path, text)) = read(paths)? else { return Ok(Preferences::default()) };
    read_preferences(&text).map_err(|(line, message)| invalid(&path, line, &message))
}

pub fn save_preferences(paths: &Paths, preferences: &Preferences, writes: &Writes) -> Result<(PathBuf, bool), Error> {
    save(paths, writes, "preferences block", |text| {
        let edited = if preferences.is_empty() {
            without_block(text, "preferences")
        } else {
            with_block(text, "preferences", &block(preferences))
        };
        (read_preferences(&edited).ok() == Some(*preferences)).then_some(edited)
    })
}

/// What `updates:` in config.yaml asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Show a notice when a newer version is available
    Notify,
    /// Install newer versions in the background, and switch to them when nothing is playing
    #[default]
    Auto,
    /// Never check for newer versions
    Off,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }
}

pub fn updates(paths: &Paths) -> Result<Policy, Error> {
    let Some((path, text)) = read(paths)? else { return Ok(Policy::default()) };
    read_updates(&text).map_err(|(line, message)| invalid(&path, line, &message))
}

pub fn save_updates(paths: &Paths, policy: Policy, writes: &Writes) -> Result<(PathBuf, bool), Error> {
    save(paths, writes, "updates line", |text| {
        let edited = with_block(text, "updates", &format!("updates: {}\n", policy.as_str()));
        (read_updates(&edited).ok() == Some(policy)).then_some(edited)
    })
}

/// Writes `server: true` into config.yaml, or takes the line out.
pub fn save_server(paths: &Paths, on: bool, writes: &Writes) -> Result<(PathBuf, bool), Error> {
    save(paths, writes, "server line", |text| {
        let edited = if on { with_block(text, "server", "server: true\n") } else { without_block(text, "server") };
        (read_server(&edited).ok() == Some(on)).then_some(edited)
    })
}

/// The Torznab indexers in config.yaml. The rest of the file need not be valid.
pub fn indexers(paths: &Paths) -> Result<Indexers, Error> {
    let Some((path, text)) = read(paths)? else { return Ok(Indexers::default()) };
    read_indexers(&text).map_err(|(line, message)| invalid(&path, line, &message))
}

/// Writes `indexers` into config.yaml. It keeps every line outside `sources`, and in the list
/// each entry that stays as it was, with its comments. The file becomes readable by this user
/// alone while it holds a key.
pub fn save_indexers(paths: &Paths, indexers: &Indexers, writes: &Writes) -> Result<(PathBuf, bool), Error> {
    save(paths, writes, "sources block", |text| {
        let wanted = |edited: &String| read_indexers(edited).ok().as_ref() == Some(indexers);
        edit_torznab(text, &indexers.entries).filter(wanted).or_else(|| {
            let whole = if indexers.entries.is_empty() && indexers.disabled.is_empty() {
                without_block(text, "sources")
            } else {
                with_block(text, "sources", &sources_block(indexers))
            };
            wanted(&whole).then_some(whole)
        })
    })
}

fn read_indexers(text: &str) -> Result<Indexers, Problem> {
    let file: SourcesOnly = serde_saphyr::from_str(text).map_err(|e| problem(&e))?;
    let entries = file.sources.torznab.into_iter().map(Torznab::entry).collect::<Result<_, _>>()?;
    Ok(Indexers { entries, disabled: file.sources.disabled })
}

/// Whether `text` holds an API key, or may: an unreadable `sources` block counts.
fn holds_key(text: &str) -> bool {
    read_indexers(text).map_or(true, |indexers| indexers.entries.iter().any(|e| matches!(e.key, Key::Inline(_))))
}

fn save(
    paths: &Paths,
    writes: &Writes,
    part: &str,
    edit: impl FnOnce(&str) -> Option<String>,
) -> Result<(PathBuf, bool), Error> {
    let (path, text) = read(paths)?.unwrap_or_else(|| (path(paths), String::new()));
    let Some(edited) = edit(&text) else {
        return Err(invalid(&path, None, &format!("setup cannot rewrite the {part} in this file")));
    };
    if edited == text {
        return Ok((path, false));
    }
    let target = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| writes.error(dir, e))?;
    }
    let write = if holds_key(&edited) { write_private } else { write_atomic };
    write(&target, edited.as_bytes()).map_err(|e| writes.error(&target, e))?;
    Ok((path, true))
}

/// A YAML scalar for `text`: bare when nothing in it could read as anything else, else quoted.
fn scalar(text: &str) -> String {
    let plain = text.starts_with(|c: char| c.is_ascii_alphabetic())
        && text.chars().all(|c| c.is_ascii_alphanumeric() || "._/:?&=%~+@-".contains(c))
        && !["true", "false", "yes", "no", "on", "off", "null", "y", "n"].contains(&text.to_ascii_lowercase().as_str());
    if plain { text.to_string() } else { format!("'{}'", text.replace('\'', "''")) }
}

/// One `sources.torznab` entry, its dash at `indent` spaces.
fn torznab_item(entry: &IndexerEntry, indent: usize) -> String {
    let (dash, inner) = (" ".repeat(indent), " ".repeat(indent + 2));
    let key = match &entry.key {
        Key::None => String::new(),
        Key::Inline(key) => format!("{inner}api_key: {}\n", scalar(key.expose())),
        Key::Env(var) => format!("{inner}api_key_env: {}\n", scalar(var)),
    };
    format!("{dash}- name: {}\n{inner}url: {}\n{key}", scalar(&entry.name), scalar(&entry.url))
}

fn sources_block(indexers: &Indexers) -> String {
    let mut block = "sources:\n".to_string();
    if !indexers.disabled.is_empty() {
        let names: Vec<String> = indexers.disabled.iter().map(|name| scalar(name)).collect();
        block.push_str(&format!("  disabled: [{}]\n", names.join(", ")));
    }
    if !indexers.entries.is_empty() {
        block.push_str("  torznab:\n");
        block.extend(indexers.entries.iter().map(|entry| torznab_item(entry, 4)));
    }
    block
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// A line that holds YAML: not blank and not only a comment.
fn is_content(line: &str) -> bool {
    !line.trim().is_empty() && !is_comment(line)
}

/// Whether `line` is `key:` with nothing after it but a comment.
fn opens(line: &str, key: &str) -> bool {
    line.trim_start().strip_prefix(key).and_then(|rest| rest.strip_prefix(':')).is_some_and(|rest| {
        let rest = rest.trim();
        rest.is_empty() || rest.starts_with('#')
    })
}

/// `text` with `sources.torznab` holding `entries`, edited line by line: entries that stay keep
/// their lines and the comments above them. `None` when the block is in a shape this does not
/// edit, such as a flow list.
fn edit_torznab(text: &str, entries: &[IndexerEntry]) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some((start, end)) = block_lines(&lines, "sources") else {
        if entries.is_empty() {
            return Some(text.to_string());
        }
        let items: String = entries.iter().map(|entry| torznab_item(entry, 4)).collect();
        return Some(with_block(text, "sources", &format!("sources:\n  torznab:\n{items}")));
    };
    if !opens(lines[start], "sources") {
        return None;
    }
    let child = lines[start + 1..end].iter().find(|line| is_content(line)).map(|line| indent(line))?;
    let named = |i: &usize| indent(lines[*i]) == child && lines[*i].trim_start().starts_with("torznab:");
    let Some(key) = (start + 1..end).find(named) else {
        if entries.is_empty() {
            return Some(text.to_string());
        }
        let items: String = entries.iter().map(|entry| torznab_item(entry, child + 2)).collect();
        let added = format!("{}torznab:\n{items}", " ".repeat(child));
        return Some([ended(lines[..end].concat()), added, lines[end..].concat()].concat());
    };
    if !opens(lines[key], "torznab") {
        return None;
    }
    // The list runs to its last line of YAML: deeper than the key, or a dash beside it.
    let inside = |line: &str| is_content(line) && (indent(line) > child || line.trim_start().starts_with('-'));
    let last =
        (key + 1..end).take_while(|&i| !is_content(lines[i]) || inside(lines[i])).filter(|&i| inside(lines[i])).last();
    let stop = last.map_or(key + 1, |last| last + 1);
    let dash = (key + 1..stop).map(|i| lines[i]).find(|line| line.trim_start().starts_with('-')).map(indent);
    let starts: Vec<usize> =
        (key + 1..stop).filter(|&i| Some(indent(lines[i])) == dash && lines[i].trim_start().starts_with('-')).collect();
    // Each entry takes the comments right above its dash with it.
    let bounds: Vec<usize> = (0..starts.len())
        .map(|n| {
            let floor = if n == 0 { key + 1 } else { starts[n - 1] + 1 };
            let mut from = starts[n];
            while from > floor && is_comment(lines[from - 1]) {
                from -= 1;
            }
            from
        })
        .collect();
    let mut kept: Vec<(Option<IndexerEntry>, String)> = Vec::new();
    for (n, &from) in bounds.iter().enumerate() {
        let to = bounds.get(n + 1).copied().unwrap_or(stop);
        let chunk = ended(lines[from..to].concat());
        let dedented: String = lines[from..to]
            .iter()
            .map(|&line| line.get(dash.unwrap_or(0).min(indent(line))..).unwrap_or(line))
            .collect();
        let parsed: Option<Vec<Torznab>> = serde_saphyr::from_str(&dedented).ok();
        let entry = parsed.filter(|items| items.len() == 1).and_then(|mut items| items.pop()?.entry().ok());
        kept.push((entry, chunk));
    }
    if entries.is_empty() {
        let rest = [lines[start..key].concat(), lines[stop..end].concat()].concat();
        if !rest.lines().skip(1).any(is_content) {
            return Some(without_block(text, "sources"));
        }
        return Some([lines[..key].concat(), lines[stop..].concat()].concat());
    }
    let first = bounds.first().copied().unwrap_or(stop);
    let mut list = ended(lines[key..first].concat());
    for entry in entries {
        match kept.iter().position(|(was, _)| was.as_ref() == Some(entry)) {
            Some(at) => list.push_str(&kept.remove(at).1),
            None => list.push_str(&torznab_item(entry, dash.unwrap_or(child + 2))),
        }
    }
    Some([lines[..key].concat(), list, lines[stop..].concat()].concat())
}

/// `text` with a line break at its end, unless it is empty.
fn ended(mut text: String) -> String {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn read_preferences(text: &str) -> Result<Preferences, Problem> {
    let file: PreferencesOnly = serde_saphyr::from_str(text).map_err(|e| problem(&e))?;
    file.preferences.unwrap_or_default().read()
}

fn read_updates(text: &str) -> Result<Policy, Problem> {
    let file: UpdatesOnly = serde_saphyr::from_str(text).map_err(|e| problem(&e))?;
    Ok(file.updates.unwrap_or_default())
}

fn read_server(text: &str) -> Result<bool, Problem> {
    let file: ServerOnly = serde_saphyr::from_str(text).map_err(|e| problem(&e))?;
    Ok(file.server.unwrap_or_default())
}

impl Overrides {
    fn read(self) -> Result<Preferences, Problem> {
        let max_size = match self.max_size {
            Some(Spanned { value, referenced, .. }) if value.trim() != "none" => {
                Some(parse_size(&value).ok_or_else(|| {
                    let message = format!("preferences.max_size {value:?} is not a size; use a form like 8GB, or none");
                    (Some(referenced.line()), message)
                })?)
            }
            _ => None,
        };
        Ok(Preferences { screen: self.screen, max_size, playback: self.playback })
    }
}

fn block(preferences: &Preferences) -> String {
    let lines: Vec<String> = [
        preferences.screen.map(|s| format!("  screen: {}", s.as_str())),
        preferences.max_size.map(|max| format!("  max_size: {}", size_text(max))),
        preferences.playback.map(|p| format!("  playback: {}", p.as_str())),
    ]
    .into_iter()
    .flatten()
    .collect();
    format!("preferences:\n{}\n", lines.join("\n"))
}

fn block_lines(lines: &[&str], key: &str) -> Option<(usize, usize)> {
    let is_key = |line: &&str| line.strip_prefix(key).is_some_and(|rest| rest.starts_with(':'));
    let start = lines.iter().position(is_key)?;
    let mut end = start + 1;
    while end < lines.len() && (lines[end].starts_with([' ', '\t']) || lines[end].trim().is_empty()) {
        end += 1;
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some((start, end))
}

fn without_block(text: &str, key: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some((start, mut end)) = block_lines(&lines, key) else { return text.to_string() };
    while end < lines.len() && lines[end].trim().is_empty() {
        end += 1;
    }
    let mut before = lines[..start].concat();
    if end == lines.len() {
        before.truncate(before.trim_end().len());
        if !before.is_empty() {
            before.push('\n');
        }
    }
    before + &lines[end..].concat()
}

fn with_block(text: &str, key: &str, block: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let Some((start, end)) = block_lines(&lines, key) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.trim().is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(block);
        return out;
    };
    [lines[..start].concat(), block.to_string(), lines[end..].concat()].concat()
}

fn read(paths: &Paths) -> Result<Option<(PathBuf, String)>, Error> {
    let try_read = |path: PathBuf| match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((path, text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(invalid(&path, None, &format!("cannot be read: {e}"))),
    };
    let toml = paths.config.join("config.toml");
    match (try_read(path(paths))?, try_read(paths.config.join("config.yml"))?) {
        (Some((yaml, _)), Some((yml, _))) => Err(Error::new(
            ErrorCode::Usage,
            format!("{} and {} both exist, and only one can be used", yaml.display(), yml.display()),
        )
        .with_hint("move what you want to keep into one file and delete the other")),
        (None, None) if toml.exists() => Err(Error::new(
            ErrorCode::Usage,
            format!("{} is not supported; the config file is config.yaml", toml.display()),
        )
        .with_hint(format!("write your settings as YAML in {}, then delete config.toml", path(paths).display()))),
        (found, None) | (None, found) => Ok(found),
    }
}

fn problem(error: &serde_saphyr::Error) -> Problem {
    (error.location().map(|at| at.line()), describe(error))
}

fn parse(text: &str, env: &Env) -> Result<Config, Problem> {
    let file: File = serde_saphyr::from_str(text).map_err(|e| problem(&e))?;
    let downloads = file
        .downloads
        .dir
        .map(|Spanned { value: raw, referenced, .. }| {
            let expanded = env.expand_home(Path::new(&raw));
            if expanded.is_absolute() {
                Ok(expanded)
            } else {
                Err((
                    Some(referenced.line()),
                    format!("downloads.dir {raw:?} must be an absolute path or start with ~/"),
                ))
            }
        })
        .transpose()?;
    let torznab = file
        .sources
        .torznab
        .into_iter()
        .map(|torznab| {
            let line = torznab.api_key_env.as_ref().map(|var| var.referenced.line());
            let IndexerEntry { name, url, key } = torznab.entry()?;
            let api_key = match key {
                Key::None => None,
                Key::Inline(key) => Some(key.expose().to_string()),
                Key::Env(var) => Some(env.var(&var).map(str::to_string).ok_or_else(|| {
                    let message = format!(
                        "the {name:?} indexer takes its API key from {var}, which is not set for the engine; write the key as api_key instead"
                    );
                    (line, message)
                })?),
            };
            Ok(TorznabConfig { name, url, api_key })
        })
        .collect::<Result<_, _>>()?;
    let search = SearchConfig { torznab, disabled: file.sources.disabled, ..SearchConfig::default() };
    let preferences = file.preferences.unwrap_or_default().read()?;
    let player = file
        .player
        .map(|Spanned { value, referenced, .. }| {
            Choice::parse(&value).ok_or_else(|| {
                let message = format!("player {value:?} is not iina, mpv, vlc, or an absolute path");
                (Some(referenced.line()), message)
            })
        })
        .transpose()?;
    let subtitles = SubtitleConfig { online: file.subtitles.online.unwrap_or(true), ..SubtitleConfig::default() };
    let catalog = CatalogConfig {
        online: file.catalog.online.unwrap_or(true),
        endpoint: file.catalog.endpoint,
        imdb_endpoint: None,
        cache_dir: None,
    };
    let ffmpeg = FfmpegConfig { fetch: file.ffmpeg.fetch.unwrap_or(true) };
    let updates = file.updates.unwrap_or_default();
    let plays_max = match file.plays.max {
        None => PlaysMax::default(),
        Some(Spanned { value, .. }) if value.trim() == "none" => PlaysMax(u64::MAX),
        Some(Spanned { value, referenced, .. }) => PlaysMax(parse_size(&value).ok_or_else(|| {
            (Some(referenced.line()), format!("plays.max {value:?} is not a size; use a form like 20GiB, or none"))
        })?),
    };
    let public_address = file
        .public_address
        .map(|Spanned { value, referenced, .. }| {
            PublicAddress::parse(&value).ok_or_else(|| {
                let message = format!(
                    "public_address {value:?} is not an address such as https://tv.example.com or http://box.tailnet.ts.net:47919"
                );
                (Some(referenced.line()), message)
            })
        })
        .transpose()?;
    let exposure = Exposure { server: file.server.unwrap_or_default(), public_address };
    let torrent_interface =
        file.torrent_interface.map(|Spanned { value, referenced, .. }| torrent_interface(&value, referenced.line()));
    Ok(Config {
        downloads,
        search,
        file: None,
        preferences,
        updates,
        player,
        subtitles,
        catalog,
        ffmpeg,
        plays_max,
        exposure,
        torrent_interface: torrent_interface.transpose()?,
    })
}

#[cfg(unix)]
fn torrent_interface(name: &str, line: u64) -> Result<InterfaceName, Problem> {
    InterfaceName::new(name).ok_or_else(|| {
        let message = format!("torrent_interface {name:?} is not a network interface name; use one like wg0");
        (Some(line), message)
    })
}

/// libtorrent names Windows adapters by GUID and binds to their addresses rather than to them, so
/// torrent traffic could still leave by another adapter.
#[cfg(windows)]
fn torrent_interface(_: &str, line: u64) -> Result<InterfaceName, Problem> {
    Err((Some(line), "torrent_interface works on Linux and macOS, not on Windows".to_string()))
}

fn describe(error: &serde_saphyr::Error) -> String {
    struct NoLocation;
    impl Localizer for NoLocation {
        fn attach_location<'a>(&self, base: Cow<'a, str>, _: Location) -> Cow<'a, str> {
            base
        }
    }
    let formatter = UserMessageFormatter.with_localizer(&NoLocation);
    error.render_with_options(serde_saphyr::render_options! { formatter: &formatter, snippets: SnippetMode::Off })
}

fn invalid(path: &Path, line: Option<u64>, message: &str) -> Error {
    let place = match line {
        Some(line) => format!("{} line {line}", path.display()),
        None => path.display().to_string(),
    };
    Error::new(ErrorCode::Usage, format!("{place}: {message}")).with_hint(format!("fix {}", path.display()))
}

/// Replacement base URLs for tests, as `name=url` pairs separated by commas in the variable `key`.
#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = if cfg!(windows) { r"C:\Users\ada" } else { "/Users/ada" };
    const PLAYER: &str = if cfg!(windows) { r"C:\Tools\play.exe" } else { "/usr/local/bin/play" };

    fn env() -> Env {
        Env::from_pairs([("HOME", HOME), ("JACKETT_KEY", "k3y")])
    }

    fn load(text: &str) -> Result<Config, Error> {
        load_files(&[("config.yaml", text)])
    }

    fn load_files(files: &[(&str, &str)]) -> Result<Config, Error> {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let env = Env::from_pairs([("HOME", HOME), ("TORRENTIAL_HOME", home.as_str())]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        std::fs::create_dir_all(&paths.config).unwrap();
        for (name, text) in files {
            std::fs::write(paths.config.join(name), text).unwrap();
        }
        // With / between a path's parts on every platform, so one message is expected everywhere.
        let relative = |text: String| text.replace(&home, "$TORRENTIAL_HOME").replace('\\', "/");
        Config::load(&paths, &env)
            .map(|config| Config { file: config.file.map(|f| relative(f.display().to_string()).into()), ..config })
            .map_err(|e| Error { message: relative(e.message), hint: e.hint.map(relative), ..e })
    }

    #[test]
    fn reads_downloads_disabled_sources_and_indexers() {
        let text = "
downloads:
  dir: ~/isos
sources:
  disabled: [nyaa, apibay]
  torznab:
    - name: jackett
      url: http://127.0.0.1:9117/api
      api_key_env: JACKETT_KEY
";
        let config = parse(text, &env()).unwrap();
        assert_eq!(config.downloads, Some(Path::new(HOME).join("isos")));
        assert_eq!(config.search.disabled, ["nyaa", "apibay"]);
        assert_eq!(
            config.search.torznab,
            [TorznabConfig {
                name: "jackett".into(),
                url: "http://127.0.0.1:9117/api".into(),
                api_key: Some("k3y".into())
            }]
        );
        assert_eq!(parse("", &env()).unwrap(), Config::default());
        assert_eq!(parse("# nothing yet\n", &env()).unwrap(), Config::default());
    }

    #[test]
    fn reads_config_yml_only_when_config_yaml_is_absent() {
        let text = "sources:\n  disabled: [nyaa]\n";
        let yaml = load_files(&[("config.yaml", text)]).unwrap();
        let yml = load_files(&[("config.yml", text)]).unwrap();
        let neither = load_files(&[]).unwrap();
        let both = load_files(&[("config.yaml", text), ("config.yml", text)]).unwrap_err();

        assert_eq!(
            [yaml, yml].map(|c| (c.file.unwrap().display().to_string(), c.search.disabled)),
            [
                ("$TORRENTIAL_HOME/config/config.yaml".to_string(), vec!["nyaa".to_string()]),
                ("$TORRENTIAL_HOME/config/config.yml".to_string(), vec!["nyaa".to_string()]),
            ]
        );
        assert_eq!(neither, Config::default());
        assert_eq!(
            (both.code, both.message.as_str(), both.hint.as_deref()),
            (
                ErrorCode::Usage,
                "$TORRENTIAL_HOME/config/config.yaml and $TORRENTIAL_HOME/config/config.yml both exist, and only one can be \
                 used",
                Some("move what you want to keep into one file and delete the other")
            )
        );
    }

    #[test]
    fn a_config_toml_is_an_error_until_a_yaml_config_exists() {
        let toml = "[downloads]\ndir = \"~/isos\"\n";
        let alone = load_files(&[("config.toml", toml)]).unwrap_err();
        let beside = load_files(&[("config.toml", toml), ("config.yaml", "sources:\n  disabled: [nyaa]\n")]).unwrap();

        assert_eq!(
            (alone.code, alone.message.as_str(), alone.hint.as_deref()),
            (
                ErrorCode::Usage,
                "$TORRENTIAL_HOME/config/config.toml is not supported; the config file is config.yaml",
                Some("write your settings as YAML in $TORRENTIAL_HOME/config/config.yaml, then delete config.toml")
            )
        );
        assert_eq!(beside.search.disabled, ["nyaa"]);
    }

    #[test]
    fn reads_preferences_and_an_absent_or_empty_block_as_none() {
        let all = "preferences:\n  screen: tv-4k\n  max_size: 8GB\n  playback: hevc-hdr\n";
        let read = |text: &str| parse(text, &env()).unwrap().preferences;
        assert_eq!(
            read(all),
            Preferences {
                screen: Some(Screen::Tv4k),
                max_size: Some(8_000_000_000),
                playback: Some(Playback::HevcHdr)
            }
        );
        for none in ["preferences: {}\n", "preferences:\n", "preferences:\n  max_size: none\n", "sources: {}\n"] {
            assert_eq!(read(none), Preferences::default(), "{none}");
        }
    }

    #[test]
    fn the_plays_folder_holds_20_gib_unless_config_yaml_says_otherwise() {
        let max = |text: &str| parse(text, &env()).map(|c| c.plays_max.0);
        assert_eq!(max(""), Ok(20 << 30));
        assert_eq!(max("plays:\n  max: 8GiB\n"), Ok(8 << 30));
        assert_eq!(max("plays: { max: 700MB }\n"), Ok(700_000_000));
        assert_eq!(max("plays:\n  max: none\n"), Ok(u64::MAX));
        assert_eq!(
            max("updates: auto\nplays:\n  max: lots\n"),
            Err((Some(3), "plays.max \"lots\" is not a size; use a form like 20GiB, or none".to_string()))
        );
    }

    #[test]
    fn torrent_interface_names_one_interface() {
        let read = |text: &str| parse(text, &env()).map(|c| c.torrent_interface.map(|name| name.to_string()));
        assert_eq!(read(""), Ok(None));
        if cfg!(windows) {
            let refused = "torrent_interface works on Linux and macOS, not on Windows".to_string();
            assert_eq!(read("torrent_interface: wg0\n"), Err((Some(1), refused)));
            return;
        }
        assert_eq!(read("torrent_interface: wg0\n"), Ok(Some("wg0".to_string())));
        assert_eq!(
            read("updates: auto\ntorrent_interface: wg0,eth0\n"),
            Err((
                Some(2),
                "torrent_interface \"wg0,eth0\" is not a network interface name; use one like wg0".to_string()
            ))
        );
    }

    #[test]
    fn bad_preferences_name_the_line() {
        let cases = [
            (
                "preferences:\n  screen: tv4k\n",
                "line 2: unknown variant `tv4k`, expected one of phone, laptop, tv, tv-4k",
            ),
            (
                "preferences:\n  max_size: lots\n",
                "line 2: preferences.max_size \"lots\" is not a size; use a form like 8GB, or none",
            ),
            (
                "preferences:\n  speakers: 5.1\n",
                "line 2: unknown field `speakers`, expected one of screen, max_size, playback",
            ),
        ];
        for (text, expected) in cases {
            let error = load(text).unwrap_err();
            assert_eq!(error.message, format!("$TORRENTIAL_HOME/config/config.yaml {expected}"), "{text}");
        }
    }

    #[test]
    fn reads_the_player_and_names_the_line_of_a_bad_one() {
        let player = |text: &str| parse(text, &env()).map(|c| c.player);
        assert_eq!(player("player: mpv\n"), Ok(Some(Choice::Mpv)));
        assert_eq!(player(&format!("player: {PLAYER}\n")), Ok(Some(Choice::Program(PLAYER.into()))));
        assert_eq!(player("updates: auto\n"), Ok(None));
        assert_eq!(
            player("updates: auto\nplayer: celluloid\n"),
            Err((Some(2), "player \"celluloid\" is not iina, mpv, vlc, or an absolute path".to_string()))
        );
    }

    struct Folder {
        _dir: tempfile::TempDir,
        paths: Paths,
    }

    impl Folder {
        fn new() -> Folder {
            let dir = tempfile::tempdir().unwrap();
            let env = Env::from_pairs([("HOME", HOME), ("TORRENTIAL_HOME", dir.path().to_str().unwrap())]);
            let paths = Paths::resolve(&env, 501, None).unwrap();
            Folder { _dir: dir, paths }
        }

        fn write(&self, text: &str) {
            std::fs::create_dir_all(&self.paths.config).unwrap();
            std::fs::write(path(&self.paths), text).unwrap();
        }

        fn text(&self) -> String {
            std::fs::read_to_string(path(&self.paths)).unwrap()
        }

        fn save(&self, preferences: Preferences) -> bool {
            let writes = Writes { sandboxed: false, rerun: "torrential setup".into() };
            save_preferences(&self.paths, &preferences, &writes).unwrap().1
        }
    }

    const LAPTOP: Preferences =
        Preferences { screen: Some(Screen::Laptop), max_size: Some(2_000_000_000), playback: Some(Playback::H264) };

    #[test]
    fn setup_replaces_only_the_preferences_block_and_keeps_comments() {
        let folder = Folder::new();
        folder.write(
            "# where films go\ndownloads:\n  dir: ~/films # big disk\n\npreferences:\n  # old answers\n  screen: tv\n  \
             max_size: 20GB\n\n# indexers\nsources:\n  disabled: [nyaa]\n",
        );

        let changed = folder.save(LAPTOP);

        assert!(changed);
        assert_eq!(
            folder.text(),
            "# where films go\ndownloads:\n  dir: ~/films # big disk\n\npreferences:\n  screen: laptop\n  \
             max_size: 2GB\n  playback: h264\n\n# indexers\nsources:\n  disabled: [nyaa]\n"
        );
        assert_eq!(preferences(&folder.paths).unwrap(), LAPTOP);
        assert!(!folder.save(LAPTOP), "saving the same preferences again changes nothing");
    }

    #[test]
    fn setup_adds_the_block_after_the_last_line_or_creates_the_file() {
        let existing = Folder::new();
        existing.write("sources:\n  disabled: [nyaa] # too slow");
        let missing = Folder::new();

        existing.save(LAPTOP);
        missing.save(LAPTOP);

        let block = "preferences:\n  screen: laptop\n  max_size: 2GB\n  playback: h264\n";
        assert_eq!(existing.text(), format!("sources:\n  disabled: [nyaa] # too slow\n\n{block}"));
        assert_eq!(missing.text(), block);
    }

    #[test]
    fn clearing_every_preference_removes_the_block_and_its_blank_line() {
        let cases = [
            ("preferences: {screen: tv}\ndownloads:\n  dir: /films\n", "downloads:\n  dir: /films\n"),
            (
                "downloads:\n  dir: /films\n\npreferences:\n  screen: tv\n\n# indexers\nsources: {}\n",
                "downloads:\n  dir: /films\n\n# indexers\nsources: {}\n",
            ),
            ("downloads:\n  dir: /films\n\npreferences:\n  screen: tv\n", "downloads:\n  dir: /films\n"),
            ("preferences:\n  screen: tv\n", ""),
        ];
        for (text, cleared) in cases {
            let folder = Folder::new();
            folder.write(text);
            assert!(folder.save(Preferences::default()), "{text}");
            assert_eq!(folder.text(), cleared, "{text}");
            assert_eq!(preferences(&folder.paths).unwrap(), Preferences::default());
        }
        let missing = Folder::new();
        assert!(!missing.save(Preferences::default()), "clearing a config that does not exist writes nothing");
    }

    // Windows makes links only for an administrator or in Developer Mode.
    #[cfg(unix)]
    #[test]
    fn setup_writes_through_a_linked_config() {
        let folder = Folder::new();
        let dotfiles = folder.paths.config.parent().unwrap().join("dotfiles.yaml");
        std::fs::create_dir_all(&folder.paths.config).unwrap();
        std::fs::write(&dotfiles, "sources:\n  disabled: [nyaa]\n").unwrap();
        std::os::unix::fs::symlink(&dotfiles, path(&folder.paths)).unwrap();

        folder.save(LAPTOP);

        assert!(path(&folder.paths).symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(&dotfiles).unwrap(),
            "sources:\n  disabled: [nyaa]\n\npreferences:\n  screen: laptop\n  max_size: 2GB\n  playback: h264\n"
        );
    }

    // It shuts a folder with permission bits, which Windows does not have.
    #[cfg(unix)]
    #[test]
    fn setup_keeps_the_config_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let folder = Folder::new();
        folder.write("sources:\n  disabled: [nyaa]\n");
        let config = path(&folder.paths);
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

        folder.save(LAPTOP);

        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!((format!("{mode:o}"), preferences(&folder.paths).unwrap()), ("600".to_string(), LAPTOP));
    }

    #[test]
    fn online_subtitles_are_on_unless_the_file_turns_them_off() {
        let online = |text: &str| parse(text, &env()).unwrap().subtitles.online;
        assert_eq!(
            [online(""), online("subtitles:\n  online: true\n"), online("subtitles:\n  online: false\n")],
            [true, true, false]
        );
        assert!(online("subtitles: {}\nupdates: auto\n"));
        let error = load("subtitles:\n  online: sometimes\n").unwrap_err();
        assert!(error.message.starts_with("$TORRENTIAL_HOME/config/config.yaml line 2: "), "{}", error.message);
        let error = load("subtitles:\n  offline: true\n").unwrap_err();
        assert!(error.message.contains("unknown field `offline`"), "{}", error.message);
    }

    #[test]
    fn fetching_ffmpeg_is_on_unless_the_file_turns_it_off() {
        let fetch = |text: &str| parse(text, &env()).unwrap().ffmpeg.fetch;
        assert_eq!(
            [fetch(""), fetch("ffmpeg:\n  fetch: true\n"), fetch("ffmpeg:\n  fetch: false\n")],
            [true, true, false]
        );
        assert!(Config::default().ffmpeg.fetch);
        let error = load("ffmpeg:\n  fetch: never\n").unwrap_err();
        assert!(error.message.starts_with("$TORRENTIAL_HOME/config/config.yaml line 2: "), "{}", error.message);
    }

    #[test]
    fn title_lookups_are_on_unless_the_file_turns_them_off_and_may_use_a_mirror() {
        let catalog = |text: &str| {
            let config = parse(text, &env()).unwrap().catalog;
            (config.online, config.endpoint, config.cache_dir)
        };
        assert_eq!(catalog(""), (true, None, None));
        assert_eq!(catalog("catalog:\n  online: false\n"), (false, None, None));
        assert_eq!(
            catalog("catalog:\n  endpoint: https://cinemeta.example.org\n"),
            (true, Some("https://cinemeta.example.org".into()), None)
        );
        let error = load("catalog:\n  offline: true\n").unwrap_err();
        assert!(error.message.contains("unknown field `offline`"), "{}", error.message);

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let test = "http://127.0.0.1:9/";
        let env = Env::from_pairs([("TORRENTIAL_HOME", home.as_str()), (hooks::CATALOG_ENDPOINT, test)]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        assert_eq!(Config::load(&paths, &env).unwrap().catalog.endpoint, None, "only a test process reads it");
        let env = env.with(hooks::NETWORK, "loopback");
        assert_eq!(Config::load(&paths, &env).unwrap().catalog.endpoint.as_deref(), Some(test));
    }

    #[test]
    fn updates_install_by_themselves_unless_the_file_says_notify_or_off() {
        let read = |text: &str| parse(text, &env()).unwrap().updates;
        assert_eq!(
            [read(""), read("sources: {}\n"), read("updates: notify\n"), read("updates: off\n")],
            [Policy::Auto, Policy::Auto, Policy::Notify, Policy::Off]
        );
        let error = load("sources: {}\nupdates: always\n").unwrap_err();
        assert_eq!(
            error.message,
            "$TORRENTIAL_HOME/config/config.yaml line 2: unknown variant `always`, expected one of notify, auto, off"
        );
    }

    #[test]
    fn setup_writes_the_updates_line_and_keeps_the_rest() {
        let folder = Folder::new();
        folder.write("# indexers\nsources:\n  disabled: [nyaa]\n");
        let writes = Writes { sandboxed: false, rerun: "torrential setup".into() };
        let save = |policy| save_updates(&folder.paths, policy, &writes).unwrap().1;

        assert!(save(Policy::Auto));
        assert_eq!(folder.text(), "# indexers\nsources:\n  disabled: [nyaa]\n\nupdates: auto\n");
        assert!(save(Policy::Off));
        assert_eq!(folder.text(), "# indexers\nsources:\n  disabled: [nyaa]\n\nupdates: off\n");
        assert!(!save(Policy::Off), "saving the same policy again changes nothing");
        assert_eq!(updates(&folder.paths).unwrap(), Policy::Off);
    }

    #[test]
    fn server_install_writes_the_server_line_and_uninstall_takes_it_out() {
        let folder = Folder::new();
        folder.write("# indexers\nsources:\n  disabled: [nyaa]\n");
        let writes = Writes { sandboxed: false, rerun: "torrential server install".into() };
        let save = |on| save_server(&folder.paths, on, &writes).unwrap().1;
        let server = || Config::load(&folder.paths, &env()).unwrap().exposure.server;

        assert!(!server());
        assert!(save(true));
        assert_eq!(folder.text(), "# indexers\nsources:\n  disabled: [nyaa]\n\nserver: true\n");
        assert!(server());
        assert!(!save(true), "saving it again changes nothing");
        assert!(save(false));
        assert_eq!(folder.text(), "# indexers\nsources:\n  disabled: [nyaa]\n");
        assert!(!server());
    }

    #[test]
    fn reads_the_public_address_as_an_origin() {
        let read = |text: &str| parse(&format!("public_address: {text}\n"), &env()).map(|c| c.exposure.public_address);
        let address =
            |origin: &str, host: &str, https| Some(PublicAddress { origin: origin.into(), host: host.into(), https });
        assert_eq!(read("https://TV.example.com/"), Ok(address("https://tv.example.com", "tv.example.com", true)));
        assert_eq!(read("https://tv.example.com:443"), Ok(address("https://tv.example.com", "tv.example.com", true)));
        assert_eq!(
            read("http://box.tailnet.ts.net:47919"),
            Ok(address("http://box.tailnet.ts.net:47919", "box.tailnet.ts.net:47919", false))
        );
        assert_eq!(
            read("http://[fd7a:115c:a1e0::1]:47919"),
            Ok(address("http://[fd7a:115c:a1e0::1]:47919", "[fd7a:115c:a1e0::1]:47919", false))
        );
        assert_eq!(parse("", &env()).unwrap().exposure, Exposure::default());
        for bad in [
            "tv.example.com",
            "ftp://tv.example.com",
            "https://tv.example.com/watch",
            "https://ada@tv.example.com",
            "https://tv.example.com/?a=1",
        ] {
            assert_eq!(
                read(bad),
                Err((
                    Some(1),
                    format!(
                        "public_address {bad:?} is not an address such as https://tv.example.com or http://box.tailnet.ts.net:47919"
                    )
                )),
                "{bad}"
            );
        }
    }

    #[test]
    fn names_that_yaml_1_1_reads_as_booleans_stay_strings() {
        let text = "sources:\n  disabled: [no, off, yes]\n  torznab:\n    - name: no\n      url: http://x/api\n";
        let config = parse(text, &env()).unwrap();
        assert_eq!(config.search.disabled, ["no", "off", "yes"]);
        assert_eq!(config.search.torznab[0].name, "no");
    }

    #[test]
    fn a_bad_file_is_an_error_that_names_the_line() {
        let indexer = "sources:\n  torznab:\n    - name: j\n      url: http://x/api\n";
        let cases = [
            (
                format!("downloads:\n  dir: {HOME}\nsources:\n  disable: [nyaa]\n"),
                "line 4: unknown field `disable`, expected one of disabled, torznab",
            ),
            ("sources:\n  disabled: [nyaa, apibay\n".to_string(), "line 2: unclosed bracket '['"),
            (
                "sources:\n  disabled: [nyaa]\n  disabled: [apibay]\n".to_string(),
                "line 3: duplicate mapping key: disabled not allowed here",
            ),
            ("sources:\n  torznab: 3\n".to_string(), "line 2: expected sequence start"),
            (
                "downloads:\n  dir: isos\n".to_string(),
                "line 2: downloads.dir \"isos\" must be an absolute path or start with ~/",
            ),
            (
                format!("{indexer}      api_key_env: NOPE\n"),
                "line 5: the \"j\" indexer takes its API key from NOPE, which is not set for the engine; write the key as api_key instead",
            ),
            (
                format!("{indexer}      api_key: k3y\n      api_key_env: JACKETT_KEY\n"),
                "line 5: the \"j\" indexer has both api_key and api_key_env; keep one",
            ),
            (format!("{indexer}      api_key: ''\n"), "line 5: the \"j\" indexer's api_key is empty"),
            (
                format!("{indexer}      key: k3y\n"),
                "line 5: unknown field `key`, expected one of name, url, api_key, api_key_env",
            ),
        ];
        for (text, expected) in cases {
            let error = load(&text).unwrap_err();
            assert_eq!(
                (error.code, error.message),
                (ErrorCode::Usage, format!("$TORRENTIAL_HOME/config/config.yaml {expected}")),
                "{text}"
            );
        }
    }

    fn entry(name: &str, url: &str, key: Key) -> IndexerEntry {
        IndexerEntry { name: name.into(), url: url.into(), key }
    }

    fn inline(key: &str) -> Key {
        Key::Inline(ApiKey::new(key).unwrap())
    }

    impl Folder {
        fn save_indexers(&self, entries: Vec<IndexerEntry>, disabled: &[&str]) {
            let writes = Writes { sandboxed: false, rerun: "torrential indexers add".into() };
            let disabled = disabled.iter().map(|d| d.to_string()).collect();
            save_indexers(&self.paths, &Indexers { entries, disabled }, &writes).unwrap();
        }
    }

    #[test]
    fn an_indexer_takes_its_key_from_the_file_or_from_the_engines_environment() {
        let text = "sources:\n  torznab:\n    - name: filelist\n      url: http://127.0.0.1:9696/3/api\n      api_key: k3y\n    - name: jackett\n      url: http://127.0.0.1:9117/api\n      api_key_env: JACKETT_KEY\n";
        let keys: Vec<Option<String>> =
            parse(text, &env()).unwrap().search.torznab.into_iter().map(|t| t.api_key).collect();
        assert_eq!(keys, [Some("k3y".to_string()), Some("k3y".to_string())]);
        let folder = Folder::new();
        folder.write(text);
        let listed = indexers(&folder.paths).unwrap().entries;
        assert_eq!(listed[0].key.origin(), KeyFrom::Config);
        assert_eq!(listed[1].key.origin(), KeyFrom::Env("JACKETT_KEY".into()));
        assert!(!format!("{listed:?}").contains("k3y"), "{listed:?}");
    }

    #[test]
    fn adding_and_removing_indexers_keeps_every_other_line_and_comment() {
        let folder = Folder::new();
        let start = "\
# my settings
downloads:
  dir: ~/isos   # big disk

sources:
  disabled: [nyaa]   # too slow
  torznab:
    # the Romanian one
    - name: filelist
      url: http://127.0.0.1:9696/3/api
      api_key_env: PROWLARR_KEY
# after the list
player: mpv
";
        folder.write(start);
        let filelist = entry("filelist", "http://127.0.0.1:9696/3/api", Key::Env("PROWLARR_KEY".into()));
        let tpb = entry(
            "thepiratebay",
            "http://127.0.0.1:9117/api/v2.0/indexers/thepiratebay/results/torznab/api",
            inline("0a1b"),
        );
        folder.save_indexers(vec![filelist.clone(), tpb.clone()], &["nyaa"]);
        assert_eq!(
            folder.text(),
            "\
# my settings
downloads:
  dir: ~/isos   # big disk

sources:
  disabled: [nyaa]   # too slow
  torznab:
    # the Romanian one
    - name: filelist
      url: http://127.0.0.1:9696/3/api
      api_key_env: PROWLARR_KEY
    - name: thepiratebay
      url: http://127.0.0.1:9117/api/v2.0/indexers/thepiratebay/results/torznab/api
      api_key: '0a1b'
# after the list
player: mpv
"
        );

        folder.save_indexers(vec![tpb.clone()], &["nyaa"]);
        let without = folder.text();
        assert!(!without.contains("filelist") && !without.contains("Romanian"), "{without}");
        assert!(
            without.contains("  disabled: [nyaa]   # too slow\n  torznab:\n    - name: thepiratebay\n"),
            "{without}"
        );

        folder.save_indexers(vec![], &["nyaa"]);
        assert_eq!(
            folder.text(),
            "# my settings\ndownloads:\n  dir: ~/isos   # big disk\n\nsources:\n  disabled: [nyaa]   # too slow\n# after the list\nplayer: mpv\n"
        );
    }

    #[test]
    fn a_new_list_goes_where_the_file_has_none() {
        let tpb = entry("thepiratebay", "http://127.0.0.1:9117/api", inline("k3y"));
        let folder = Folder::new();
        folder.save_indexers(vec![tpb.clone()], &[]);
        assert_eq!(
            folder.text(),
            "sources:\n  torznab:\n    - name: thepiratebay\n      url: http://127.0.0.1:9117/api\n      api_key: k3y\n"
        );
        folder.write("player: mpv\nsources:\n    disabled: [nyaa]");
        folder.save_indexers(vec![tpb.clone()], &["nyaa"]);
        assert_eq!(
            folder.text(),
            "player: mpv\nsources:\n    disabled: [nyaa]\n    torznab:\n      - name: thepiratebay\n        url: http://127.0.0.1:9117/api\n        api_key: k3y\n"
        );
        folder.write("sources: {torznab: [{name: old, url: 'http://x/api'}]}\nplayer: mpv\n");
        folder.save_indexers(vec![tpb], &[]);
        assert_eq!(
            folder.text(),
            "sources:\n  torznab:\n    - name: thepiratebay\n      url: http://127.0.0.1:9117/api\n      api_key: k3y\nplayer: mpv\n"
        );
        assert_eq!(indexers(&folder.paths).unwrap().entries[0].key, inline("k3y"));
    }

    #[test]
    fn removing_a_disabled_indexer_takes_it_out_of_disabled_too() {
        let folder = Folder::new();
        folder.write("sources:\n  disabled: [nyaa, old]\n  torznab:\n    - name: old\n      url: http://x/api\n");
        folder.save_indexers(vec![], &["nyaa"]);
        assert_eq!(folder.text(), "sources:\n  disabled: [nyaa]\n");
    }

    // It shuts a file with permission bits, which Windows does not have.
    #[cfg(unix)]
    #[test]
    fn a_file_with_a_key_is_readable_by_its_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let folder = Folder::new();
        folder.write("player: mpv\n");
        let config = path(&folder.paths);
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mode = || std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;

        folder.save_indexers(vec![entry("open", "http://127.0.0.1:3333/torznab", Key::None)], &[]);
        assert_eq!(format!("{:o}", mode()), "644", "a list with no key stays as it was");
        folder.save_indexers(vec![entry("filelist", "http://127.0.0.1:9696/3/api", inline("k3y"))], &[]);
        assert_eq!(format!("{:o}", mode()), "600");

        folder.write("sources:\n  torznab:\n    - name: j\n      url: http://x/api\n      api_key: k3y\n");
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o644)).unwrap();
        folder.save(LAPTOP);
        assert_eq!(format!("{:o}", mode()), "600", "setup closes a file that holds a key");
    }
}
