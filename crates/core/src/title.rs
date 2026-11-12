//! Reads which film or episode a video is from the names in its torrent, and tells a wanted
//! title's releases from others.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, ErrorCode};
use crate::quality::Quality;

/// A film, or with `episode` one episode of a series.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Title {
    /// The film's or the series' name as its words, without the dots and tags of a release name.
    pub name: String,
    /// The year in the name. For a series, the year it began.
    pub year: Option<u16>,
    pub episode: Option<Episode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize)]
pub struct Episode {
    pub season: u32,
    pub episode: u32,
}

/// What a file in a torrent is, most likely reading first. `path` is the file's path inside the
/// torrent, and `torrent` the torrent's name. The file's own name wins when it says enough, as
/// in a season where each file names its episode. A year that may be part of the title, as in
/// `Blade.Runner.2049.1080p`, gives a second reading.
pub fn read(path: &str, torrent: &str) -> Vec<Title> {
    let mut folders: Vec<&str> = path.split('/').collect();
    let file = Reading::of(stem(folders.pop().unwrap_or_default()));
    let around: Vec<Reading> = folders.iter().rev().map(|folder| Reading::of(folder)).collect();
    let around: Vec<Reading> = around.into_iter().chain([Reading::of(stem(torrent))]).collect();
    let readings = || std::iter::once(&file).chain(&around);
    fn named<'a>(mut from: impl Iterator<Item = &'a Reading>) -> Option<Reading> {
        from.find(|r| !r.words.is_empty()).cloned()
    }

    let season = file.season.or_else(|| around.iter().find_map(|r| r.season)).map(|(first, _)| first);
    // A bare number names an episode only next to a season, or in a fansub's `[Group] Title - 05`.
    let counted = file.counted.filter(|_| season.is_some() || file.grouped);
    if let Some(episode) = file.episode.map(|(first, _)| first).or(counted) {
        let series = if file.episode.is_some() { named(readings()) } else { named(around.iter()) };
        let series = series.or_else(|| file.counted_name());
        return series
            .map(|r| Title {
                name: r.words.join(" "),
                year: r.year,
                episode: Some(Episode { season: season.unwrap_or(1), episode }),
            })
            .into_iter()
            .collect();
    }
    let mut titles = Vec::new();
    let mut add = |title: Title| {
        if !title.name.is_empty() && !titles.contains(&title) {
            titles.push(title);
        }
    };
    let episode = |r: &Reading| Some(Episode { season: r.season.map_or(1, |(first, _)| first), episode: r.episode?.0 });
    // The first name that carries a year or an episode says more than a bare `movie.mkv`.
    if let Some(telling) = readings().find(|r| !r.words.is_empty() && (r.year.is_some() || r.episode.is_some())) {
        add(Title { name: telling.words.join(" "), year: telling.year, episode: episode(telling) });
        if let Some(with_year) = &telling.with_year {
            add(Title { name: with_year.join(" "), year: None, episode: None });
        }
    }
    for reading in readings() {
        add(Title { name: reading.words.join(" "), year: reading.year, episode: episode(reading) });
    }
    titles
}

/// A title as it compares: lowercase, without accents or punctuation, `&` as `and`.
pub fn normalized(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        match c {
            '&' => out.push_str(" and "),
            '\'' | '’' | '`' => {}
            c if c.is_alphanumeric() => out.push_str(unaccented(c).unwrap_or(c.encode_utf8(&mut [0; 4]))),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn unaccented(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => "a",
        'ç' | 'ć' | 'č' => "c",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ę' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ī' => "i",
        'ñ' | 'ń' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => "o",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => "u",
        'ý' | 'ÿ' => "y",
        'š' | 'ś' => "s",
        'ž' | 'ź' | 'ż' => "z",
        'ł' => "l",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        _ => return None,
    })
}

/// What a release holds of a title. Ordered from smallest to largest, which is also the order
/// to play from. The text form is `film`, `s3e5`, `s3`, `s1-5`, or `complete`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Span {
    Film,
    Episode(Episode),
    Season(u32),
    Seasons {
        first: u32,
        last: u32,
    },
    /// Every season.
    Complete,
}

impl Span {
    /// Whether this holds the film, for `None`, or the episode.
    pub fn covers(self, episode: Option<Episode>) -> bool {
        match (self, episode) {
            (Span::Film, None) => true,
            (Span::Episode(held), Some(episode)) => held == episode,
            (Span::Season(season), Some(episode)) => season == episode.season,
            (Span::Seasons { first, last }, Some(episode)) => (first..=last).contains(&episode.season),
            (Span::Complete, Some(_)) => true,
            _ => false,
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Span::Film => f.write_str("film"),
            Span::Episode(Episode { season, episode }) => write!(f, "s{season}e{episode}"),
            Span::Season(season) => write!(f, "s{season}"),
            Span::Seasons { first, last } => write!(f, "s{first}-{last}"),
            Span::Complete => f.write_str("complete"),
        }
    }
}

impl FromStr for Span {
    type Err = Error;

    fn from_str(text: &str) -> Result<Self, Error> {
        let number = |text: &str| text.parse::<u32>().ok().filter(|_| text.bytes().all(|b| b.is_ascii_digit()));
        let season = text.strip_prefix('s').unwrap_or_default();
        let span = match text {
            "film" => Some(Span::Film),
            "complete" => Some(Span::Complete),
            _ => match (season.split_once('e'), season.split_once('-')) {
                (Some((season, episode)), None) => number(season)
                    .zip(number(episode))
                    .map(|(season, episode)| Span::Episode(Episode { season, episode })),
                (None, Some((first, last))) => {
                    number(first).zip(number(last)).map(|(first, last)| Span::Seasons { first, last })
                }
                _ => number(season).map(Span::Season),
            },
        };
        span.ok_or_else(|| {
            Error::new(ErrorCode::Usage, format!("{text:?} is not a span"))
                .with_hint("a span is film, s3e5, s3, s1-5, or complete")
        })
    }
}

impl TryFrom<String> for Span {
    type Error = Error;

    fn try_from(text: String) -> Result<Self, Error> {
        text.parse()
    }
}

impl From<Span> for String {
    fn from(span: Span) -> String {
        span.to_string()
    }
}

/// A release's name read as a title.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Named {
    /// The title's words, as `read` gives them.
    pub name: String,
    pub year: Option<u16>,
    pub span: Span,
}

/// Countries that tell series of one name apart, as in `The Office (US)`. `tokens` drops what is
/// in brackets, so `named` puts these back.
const COUNTRIES: [&str; 5] = ["(US)", "(UK)", "(AU)", "(NZ)", "(CA)"];

/// Reads a release's name as a title, keeping the season of a pack, which `read` drops. `S02`,
/// `Season 2`, `S01-S05`, and `Complete` are packs. Several readings when a year may be part of
/// the title, as `read` gives, or when one file holds several episodes, as `S01E01E02` does.
pub fn named(release: &str) -> Vec<Named> {
    let reading = Reading::of(stem(release));
    let season = reading.season;
    let counted = reading.counted.filter(|_| season.is_some() || reading.grouped).map(|n| (n, n));
    let spans: Vec<Span> = match (reading.episode.or(counted), season) {
        (Some((first, last)), season) => {
            let season = season.map_or(1, |(season, _)| season);
            (first..=last).map(|episode| Span::Episode(Episode { season, episode })).collect()
        }
        (None, Some((first, last))) if first == last => vec![Span::Season(first)],
        (None, Some((first, last))) => vec![Span::Seasons { first, last }],
        (None, None) if reading.complete => vec![Span::Complete],
        (None, None) => vec![Span::Film],
    };
    let mut words = reading.words;
    if let Some(country) = COUNTRIES.iter().find(|country| release.contains(**country))
        && !words.is_empty()
    {
        words.push(country.trim_matches(['(', ')']).to_string());
    }
    let name = words.join(" ");
    let mut readings: Vec<Named> =
        spans.into_iter().map(|span| Named { name: name.clone(), year: reading.year, span }).collect();
    if let Some(with_year) = reading.with_year {
        readings.push(Named { name: with_year.join(" "), year: None, span: Span::Film });
    }
    readings.retain(|n| !n.name.is_empty());
    readings
}

/// Every episode a file in a torrent holds, as `read` reads it: none for a film, and several for
/// a file such as `S01E01E02`.
pub fn episodes(path: &str, torrent: &str) -> Vec<Episode> {
    let Some(first) = read(path, torrent).into_iter().find_map(|title| title.episode) else { return Vec::new() };
    let file = Reading::of(stem(path.rsplit('/').next().unwrap_or_default()));
    let last = file.episode.filter(|&(start, _)| start == first.episode).map_or(first.episode, |(_, last)| last);
    (first.episode..=last).map(|episode| Episode { season: first.season, episode }).collect()
}

/// Whether other titles of the same kind share the wanted one's name, as Dune 1984 and Dune 2021
/// do. A release that names no year can only be the one the catalog lists first.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Namesakes {
    None,
    /// Others share the name, and the catalog lists this one first.
    Leading,
    /// Another one shares the name and is listed first.
    Shadowed,
}

/// The film or episode a person wants, as the catalog names it, to tell its releases from others.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Wanted {
    pub name: String,
    /// A film's year, or the year a series began.
    pub year: Option<u16>,
    /// `None` for a film.
    pub episode: Option<Episode>,
    pub namesakes: Namesakes,
    /// A film's runtime, or an episode's.
    #[serde(rename = "runtime_s", with = "seconds", default)]
    pub runtime: Option<Duration>,
    /// Episodes in the wanted episode's season, to size a season pack per episode.
    pub season_episodes: Option<u32>,
}

impl Wanted {
    /// The reading of `release` that is this title, or `None`. The name must be the same once
    /// normalized. A film's year may be 1 off the catalog's, since a release can name the year
    /// after the premiere. A series' year must be the year it began. A release without a year
    /// fits unless another title of the name is listed first. For an episode, the release holds
    /// the episode itself or a pack that covers it.
    pub fn fit(&self, release: &str) -> Option<Named> {
        let name = normalized(&self.name);
        named(release).into_iter().find(|reading| {
            let year = match (reading.year, self.year) {
                (None, _) => self.namesakes != Namesakes::Shadowed,
                (Some(_), None) => true,
                (Some(named), Some(year)) if self.episode.is_some() => named == year,
                (Some(named), Some(year)) => named.abs_diff(year) <= 1,
            };
            year && reading.span.covers(self.episode) && normalized(&reading.name) == name
        })
    }

    /// What to search for. A film: `name year`, then `name` for releases without the year
    /// unless they cannot be this film. An episode: `name S03E05`, `name S03`, and `name complete`.
    pub fn queries(&self) -> Vec<String> {
        let name = &self.name;
        match (self.episode, self.year) {
            (Some(Episode { season, episode }), _) => {
                vec![
                    format!("{name} S{season:02}E{episode:02}"),
                    format!("{name} S{season:02}"),
                    format!("{name} complete"),
                ]
            }
            (None, Some(year)) if self.namesakes == Namesakes::Shadowed => vec![format!("{name} {year}")],
            (None, Some(year)) => vec![format!("{name} {year}"), name.clone()],
            (None, None) => vec![name.clone()],
        }
    }

    /// How many words `release` has beyond this title's own. Numbers and codes such as `5.1`
    /// and `60FPS` are not words, nor are the tags a title ends at or `Quality` reads, the words
    /// of a season or an episode, or the group after a final dash. A plain release has none. An
    /// alternate version names itself, as `Night Of The Living Dead (1968) Synthwave Horror Cut`
    /// does in three words.
    pub fn other_words(&self, release: &str) -> usize {
        const SPANS: [&str; 5] = ["season", "seasons", "episode", "complete", "series"];
        // Tags that `TAGS` leaves out because titles use them, as Charlotte's Web does. The
        // title's own words do not count here, so they are safe.
        const LOOSE_TAGS: [&str; 2] = ["web", "hd"];
        let own = normalized(&self.name);
        let own: Vec<&str> = own.split(' ').collect();
        let name = stem(release);
        let name = match name.rsplit_once('-') {
            Some((before, group)) if !group.is_empty() && group.chars().all(char::is_alphanumeric) => before,
            _ => name,
        };
        let words: Vec<String> = name
            .split(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '\'' | '’')))
            .map(str::to_lowercase)
            .filter(|token| !is_tag(token))
            .flat_map(|token| token.split('-').map(normalized).filter(|word| !word.is_empty()).collect::<Vec<_>>())
            .collect();
        // `Quality` reads `Full HD` and `H 264` from two words that say nothing alone.
        let pairs = |other: &str, pair: String| Quality::parse(&pair) != Quality::parse(other);
        let tag = |i: usize, word: &str| {
            is_tag(word)
                || Quality::parse(word) != Quality::default()
                || i > 0 && pairs(&words[i - 1], format!("{} {word}", words[i - 1]))
                || words.get(i + 1).is_some_and(|next| pairs(next, format!("{word} {next}")))
        };
        let other = |(i, word): &(usize, &String)| {
            !(word.bytes().any(|b| b.is_ascii_digit())
                || own.contains(&word.as_str())
                || SPANS.contains(&word.as_str())
                || LOOSE_TAGS.contains(&word.as_str())
                || tag(*i, word))
        };
        words.iter().enumerate().filter(other).count()
    }
}

/// An optional duration as whole seconds.
mod seconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(duration: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        duration.map(|d| d.as_secs()).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?.map(Duration::from_secs))
    }
}

const VIDEO_EXTENSIONS: [&str; 12] =
    ["mkv", "mp4", "m4v", "avi", "mov", "webm", "ts", "m2ts", "wmv", "mpg", "mpeg", "ogv"];

/// Tags that end the title in a release name. Words that are also common in titles, such as
/// `web` and `extended`, are left out: a resolution or a year almost always comes before them.
const TAGS: [&str; 44] = [
    "480p", "576p", "720p", "1080p", "1080i", "2160p", "4k", "uhd", "bluray", "blu-ray", "brrip", "bdrip", "bdremux",
    "remux", "webrip", "web-dl", "webdl", "hdtv", "pdtv", "dvdrip", "dvdscr", "hdrip", "hdcam", "hdts", "telesync",
    "x264", "x265", "h264", "h265", "hevc", "avc", "xvid", "divx", "av1", "10bit", "hdr", "hdr10", "dv", "aac", "ac3",
    "dts", "truehd", "atmos", "repack",
];

fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, extension)) if VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) => stem,
        _ => name,
    }
}

/// What one name says: a file's, a folder's, or the torrent's.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
struct Reading {
    /// The title: the words before the year, the episode, or the first tag.
    words: Vec<String>,
    year: Option<u16>,
    /// The first and last season, the same for one season.
    season: Option<(u32, u32)>,
    /// The first and last episode, the same for one episode.
    episode: Option<(u32, u32)>,
    /// The name says it holds the whole series, or the whole of its season.
    complete: bool,
    /// A number that is an episode if the names around it say this is a series: `02 - Pilot`, or
    /// the `05` of `Title - 05`.
    counted: Option<u32>,
    /// The title up to `counted`, when the number follows it.
    counted_words: Vec<String>,
    /// The name starts with a `[Group]`, as fansubs do.
    grouped: bool,
    /// The title with the year as its last word, when nothing but tags follow the year.
    with_year: Option<Vec<String>>,
}

impl Reading {
    fn of(name: &str) -> Reading {
        let (tokens, grouped) = tokens(name);
        let lower: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();
        let number = |i: usize| lower.get(i).and_then(|t| digits(t, 1, 3));
        let mut reading = Reading { grouped, ..Reading::default() };
        // Where the title ends: at the first tag or the first word about seasons and episodes.
        let mut end = tokens.len();
        let mut tags = tokens.len();
        for (i, token) in lower.iter().enumerate() {
            let found = if let Some((season, first, last)) = season_and_episode(token) {
                reading.season.get_or_insert((season, season));
                reading.episode.get_or_insert((first, last));
                true
            } else if let Some((first, last)) = seasons(&lower, i) {
                match &mut reading.season {
                    // The second half of `S01 - S05`.
                    Some((_, until)) if lower[i - 1] == "-" && first > *until => *until = last,
                    season => _ = season.get_or_insert((first, last)),
                }
                true
            } else if let Some(episode) =
                letter_number(token, "ep", 3).or(letter_number(token, "e", 3)).or(word_number(&lower, i, "episode"))
            {
                reading.episode.get_or_insert((episode, episode));
                true
            } else if token == "complete" && i > 0 && completes(&lower, i + 1) {
                reading.complete = true;
                end = end.min(if lower[i - 1] == "the" { i - 1 } else { i });
                true
            } else {
                let tag = is_tag(token);
                if tag {
                    tags = tags.min(i);
                }
                tag
            };
            if found {
                end = end.min(i);
            }
        }
        if reading.episode.is_none() {
            let dashed = (1..tags).find(|&i| lower[i] == "-" && number(i + 1).is_some() && (grouped || i + 2 == tags));
            if let Some(dash) = dashed {
                reading.counted = number(dash + 1);
                end = end.min(dash);
                reading.counted_words = words(&tokens[..end]);
            } else if end > 0 && number(0).is_some() && (end == 1 || lower[1] == "-") {
                reading.counted = number(0);
            }
        }
        let year = (1..end).rev().find_map(|i| Some((i, year(&lower[i])?)));
        match year {
            Some((at, year)) => {
                reading.year = Some(year);
                reading.words = words(&tokens[..at]);
                if at + 1 == end && reading.episode.is_none() && reading.season.is_none() && !reading.complete {
                    reading.with_year = Some(words(&tokens[..end]));
                }
            }
            None => reading.words = words(&tokens[..end]),
        }
        reading
    }

    /// The series a `Title - 05` name gives when no name around it does.
    fn counted_name(&self) -> Option<Reading> {
        (!self.counted_words.is_empty()).then(|| Reading { words: self.counted_words.clone(), ..Reading::default() })
    }
}

/// The name's words. Dots and underscores separate them, `[...]` is dropped, and `(...)` is
/// dropped but for the first year in it, as in `(Howard Hawks, 1940)`. Also whether the name
/// starts with a `[Group]`.
fn tokens(name: &str) -> (Vec<String>, bool) {
    let grouped = name.trim_start().starts_with('[');
    let mut spaced = String::new();
    let mut held = String::new();
    let mut depth = 0u32;
    for c in name.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let dated = held.split(|c: char| !c.is_alphanumeric()).find(|word| year(word).is_some());
                    if c == ')'
                        && let Some(dated) = dated
                    {
                        spaced.push_str(dated);
                    }
                    spaced.push(' ');
                    held.clear();
                }
            }
            c if depth > 0 => held.push(c),
            '.' | '_' | ',' | ':' => spaced.push(' '),
            c => spaced.push(c),
        }
    }
    (spaced.split_whitespace().map(String::from).collect(), grouped)
}

fn words(tokens: &[String]) -> Vec<String> {
    let dash = |token: &&String| token.as_str() == "-";
    let tokens: Vec<&String> = tokens.iter().skip_while(dash).collect();
    let keep = tokens.iter().rposition(|token| !dash(token)).map_or(0, |last| last + 1);
    tokens[..keep].iter().map(|token| token.to_string()).collect()
}

/// A number of `min` to `max` digits, with nothing else.
fn digits(token: &str, min: usize, max: usize) -> Option<u32> {
    let token = match token.split_once('v') {
        // A fansub's second version of an episode: `05v2`.
        Some((number, version)) if !number.is_empty() && version.len() == 1 => number,
        _ => token,
    };
    ((min..=max).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_digit())).then(|| token.parse().ok())?
}

fn year(token: &str) -> Option<u16> {
    let year: u16 = (token.len() == 4 && token.bytes().all(|b| b.is_ascii_digit())).then(|| token.parse().ok())??;
    (1900..2100).contains(&year).then_some(year)
}

/// `s01e02` and `1x02` as season, first, and last episode. The last differs in `s01e02e03`,
/// `s01e02-e03`, and `s01e02-03`.
fn season_and_episode(token: &str) -> Option<(u32, u32, u32)> {
    /// The number of up to `most` digits that starts `text`, and what follows it.
    fn number(text: &str, most: usize) -> Option<(Option<u32>, &str)> {
        let length = text.bytes().take_while(u8::is_ascii_digit).count();
        (1..=most).contains(&length).then(|| (text[..length].parse().ok(), &text[length..]))
    }
    if let Some(rest) = token.strip_prefix('s') {
        let (season, rest) = number(rest, 2)?;
        let (episode, mut rest) = number(rest.strip_prefix('e')?, 3)?;
        let (season, episode) = (season?, episode?);
        if !(rest.is_empty() || rest.starts_with(['e', '-'])) {
            return None;
        }
        let mut last = episode;
        while let Some(more) = rest.strip_prefix("-e").or(rest.strip_prefix('-')).or(rest.strip_prefix('e')) {
            match number(more, 3) {
                Some((Some(next), after)) if next > last && next - episode < 30 => (last, rest) = (next, after),
                _ => break,
            }
        }
        return Some((season, episode, last));
    }
    let (season, rest) = number(token, 2)?;
    let (episode, rest) = number(rest.strip_prefix('x')?, 3)?;
    (rest.is_empty() && token.len() >= 4).then_some((season?, episode?, episode?))
}

/// `s02`, `s01-s05`, `s01-05`, `season 2`, `season two`, and `seasons 1-5` at `i`, as the first
/// and last season.
fn seasons(tokens: &[String], i: usize) -> Option<(u32, u32)> {
    const NUMBERS: [&str; 12] =
        ["one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve"];
    let spelled = |text: &str| NUMBERS.iter().zip(1..).find(|(number, _)| **number == text).map(|(_, n)| (n, n));
    let range = |text: &str, most: usize| match text.split_once('-') {
        Some((first, last)) => {
            let (first, last) = (digits(first, 1, most)?, digits(last.strip_prefix('s').unwrap_or(last), 1, most)?);
            (first < last).then_some((first, last))
        }
        None => digits(text, 1, most).map(|season| (season, season)),
    };
    match tokens[i].as_str() {
        "season" | "seasons" => tokens.get(i + 1).and_then(|next| range(next, 3).or_else(|| spelled(next))),
        token => range(token.strip_prefix('s')?, 2),
    }
}

/// Whether `complete` before the word at `next` marks a pack, as in `Complete Series` or
/// `Complete 1080p`, rather than a title such as A Complete Unknown.
fn completes(tokens: &[String], next: usize) -> bool {
    const PACKS: [&str; 5] = ["series", "season", "seasons", "collection", "pack"];
    let Some(word) = tokens.get(next) else { return true };
    PACKS.contains(&word.as_str()) || is_tag(word) || year(word).is_some() || seasons(tokens, next).is_some()
}

fn is_tag(token: &str) -> bool {
    TAGS.contains(&token.split('-').next().unwrap_or_default()) || TAGS.contains(&token)
}

/// `e07` and `ep07` for the letters `e` and `ep`.
fn letter_number(token: &str, letters: &str, most: usize) -> Option<u32> {
    digits(token.strip_prefix(letters)?, 1, most)
}

/// The number after the word at `i`, as in `Episode 5`.
fn word_number(tokens: &[String], i: usize, word: &str) -> Option<u32> {
    (tokens[i] == word).then(|| digits(tokens.get(i + 1)?, 1, 3))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn film(name: &str, year: Option<u16>) -> Title {
        Title { name: name.into(), year, episode: None }
    }

    fn episode(name: &str, year: Option<u16>, season: u32, episode: u32) -> Title {
        Title { name: name.into(), year, episode: Some(Episode { season, episode }) }
    }

    #[test]
    fn a_film_is_its_title_and_year() {
        let cases = [
            ("Sintel.2010.1080p.mkv", "Sintel.2010.1080p.mkv", film("Sintel", Some(2010))),
            ("The.Matrix.1999.REMASTERED.1080p.BluRay.x264-GROUP.mkv", "", film("The Matrix", Some(1999))),
            ("Blade.Runner.2049.2017.2160p.UHD.BluRay.x265.mkv", "", film("Blade Runner 2049", Some(2017))),
            ("2001.A.Space.Odyssey.1968.1080p.mkv", "", film("2001 A Space Odyssey", Some(1968))),
            ("1917.2019.1080p.WEB-DL.DDP5.1.H264.mkv", "", film("1917", Some(2019))),
            ("Spider-Man.No.Way.Home.2021.1080p.WEBRip.mkv", "", film("Spider-Man No Way Home", Some(2021))),
            ("Amélie (2001) [1080p] [BluRay].mp4", "", film("Amélie", Some(2001))),
            ("Night.mp4", "Night of the Living Dead (1968) [1080p]", film("Night of the Living Dead", Some(1968))),
            ("movie.mkv", "Some.Film.2015.720p.BluRay.x264-GRP", film("Some Film", Some(2015))),
            ("300.mkv", "300.2006.1080p.BluRay.x264", film("300", Some(2006))),
            ("Film/Some_Film_(2015)_720p.avi", "Film", film("Some Film", Some(2015))),
            ("big_buck_bunny_1080p_h264.mov", "big_buck_bunny_1080p_h264.mov", film("big buck bunny", None)),
            ("Sintel.mkv", "Sintel", film("Sintel", None)),
            ("His Girl Friday (Comedy 1940) Cary Grant 720p HD", "", film("His Girl Friday", Some(1940))),
            ("Sintel (2010 - x264 2k - AAC 5.1) En sub En Fr", "", film("Sintel", Some(2010))),
            ("His Girl Friday (Howard Hawks, 1940)", "", film("His Girl Friday", Some(1940))),
        ];
        for (path, torrent, expected) in cases {
            assert_eq!(read(path, torrent).first(), Some(&expected), "{path} in {torrent}");
        }
    }

    #[test]
    fn a_year_that_may_be_the_titles_last_word_gives_two_readings() {
        assert_eq!(
            read("Blade.Runner.2049.1080p.BluRay.mkv", ""),
            [film("Blade Runner", Some(2049)), film("Blade Runner 2049", None)]
        );
        assert_eq!(read("Sintel.2010.1080p.mkv", ""), [film("Sintel", Some(2010)), film("Sintel 2010", None)]);
        assert_eq!(read("The.Matrix.1999.REMASTERED.1080p.mkv", ""), [film("The Matrix", Some(1999))]);
        assert_eq!(
            read("Night.mp4", "Night of the Living Dead (1968) [1080p]"),
            [
                film("Night of the Living Dead", Some(1968)),
                film("Night of the Living Dead 1968", None),
                film("Night", None)
            ]
        );
    }

    #[test]
    fn an_episode_is_its_series_season_and_number() {
        let cases = [
            ("Severance.S02E01.1080p.WEB.H264-GRP.mkv", "", episode("Severance", None, 2, 1)),
            ("The.Flash.2014.S01E05.720p.HDTV.x264.mkv", "", episode("The Flash", Some(2014), 1, 5)),
            ("Show.Name.1x02.Episode.Title.avi", "", episode("Show Name", None, 1, 2)),
            ("Show.S01E01E02.1080p.mkv", "", episode("Show", None, 1, 1)),
            ("S.W.A.T.2017.S01E01.720p.mkv", "", episode("S W A T", Some(2017), 1, 1)),
            ("Mr.Robot.S01.E03.720p.mkv", "", episode("Mr Robot", None, 1, 3)),
            ("Show Name - S01E02 - The Title.mkv", "Show Name", episode("Show Name", None, 1, 2)),
            // A season where the file carries the episode.
            ("Show.Name.S02E07.1080p.mkv", "Show.Name.S02.1080p.WEB-DL.x265", episode("Show Name", None, 2, 7)),
            ("Season 2/E07 - The One.mkv", "Show Name (2019)", episode("Show Name", Some(2019), 2, 7)),
            ("Season 3/Episode 05.mkv", "Breaking Bad Complete", episode("Breaking Bad", None, 3, 5)),
            ("Show Name/Season 1/02 - Pilot.mkv", "Pack", episode("Show Name", None, 1, 2)),
            ("s02e07.mkv", "Show.Name.S02.720p", episode("Show Name", None, 2, 7)),
            // Fansubs number episodes without a season.
            (
                "[SubsPlease] Sousou no Frieren - 05 (1080p) [ABCD1234].mkv",
                "[SubsPlease] Sousou no Frieren (01-28) (1080p) [Batch]",
                episode("Sousou no Frieren", None, 1, 5),
            ),
            ("[Group] Show Title S2 - 03v2 [720p].mkv", "", episode("Show Title", None, 2, 3)),
            ("video.mkv", "Show.Name.S01E02.720p.HDTV", episode("Show Name", None, 1, 2)),
        ];
        for (path, torrent, expected) in cases {
            assert_eq!(read(path, torrent).first(), Some(&expected), "{path} in {torrent}");
        }
    }

    #[test]
    fn a_number_alone_is_an_episode_only_beside_a_season() {
        assert_eq!(read("02 - Intro.mkv", "Lectures"), [film("02 - Intro", None), film("Lectures", None)]);
        assert_eq!(read("300.mkv", "300"), [film("300", None)]);
        assert_eq!(read("", ""), []);
    }

    fn spans(release: &str) -> Vec<(String, Option<u16>, String)> {
        named(release).into_iter().map(|n| (n.name, n.year, n.span.to_string())).collect()
    }

    fn reading(name: &str, year: Option<u16>, span: &str) -> (String, Option<u16>, String) {
        (name.into(), year, span.into())
    }

    #[test]
    fn a_span_reads_and_writes_its_text() {
        let all = [
            Span::Film,
            Span::Episode(Episode { season: 3, episode: 5 }),
            Span::Season(3),
            Span::Seasons { first: 1, last: 5 },
            Span::Complete,
        ];
        let json = serde_json::to_string(&all).unwrap();
        assert_eq!(json, r#"["film","s3e5","s3","s1-5","complete"]"#);
        assert_eq!(serde_json::from_str::<Vec<Span>>(&json).unwrap(), all);
        for bad in ["", "s", "e5", "s3e", "s1-", "3", "season 3", "s+3"] {
            assert_eq!(bad.parse::<Span>().unwrap_err().code, ErrorCode::Usage, "{bad}");
        }
    }

    #[test]
    fn a_pack_keeps_its_season() {
        assert_eq!(spans("Breaking.Bad.S02.1080p.BluRay.x264-ROVERS"), [reading("Breaking Bad", None, "s2")]);
        assert_eq!(spans("Breaking Bad Season 2 Complete 720p"), [reading("Breaking Bad", None, "s2")]);
        assert_eq!(spans("Show.Name.S02.1080p.WEB-DL.x265"), [reading("Show Name", None, "s2")]);
        assert_eq!(spans("Breaking.Bad.S01-S05.1080p.BluRay.x265"), [reading("Breaking Bad", None, "s1-5")]);
        assert_eq!(spans("Breaking Bad S01 - S05 720p"), [reading("Breaking Bad", None, "s1-5")]);
        assert_eq!(
            spans("Breaking Bad (2008) Season 1-5 S01-S05 Complete 1080p"),
            [reading("Breaking Bad", Some(2008), "s1-5")]
        );
        assert_eq!(spans("Breaking.Bad.The.Complete.Series.720p.BluRay"), [reading("Breaking Bad", None, "complete")]);
        assert_eq!(spans("Sherlock Complete 1080p"), [reading("Sherlock", None, "complete")]);
        assert_eq!(spans("Pioneer One Season One Complete XviD"), [reading("Pioneer One", None, "s1")]);
        assert_eq!(spans("Grey's Anatomy Season Twelve 720p"), [reading("Grey's Anatomy", None, "s12")]);
        assert_eq!(spans("Breaking.Bad.S03E05.720p.HDTV.x264-IMMERSE"), [reading("Breaking Bad", None, "s3e5")]);
        assert_eq!(
            spans("A Complete Unknown 2024 1080p WEB-DL"),
            [reading("A Complete Unknown", Some(2024), "film"), reading("A Complete Unknown 2024", None, "film")]
        );
    }

    #[test]
    fn a_file_with_several_episodes_reads_as_each() {
        assert_eq!(
            spans("Severance.S02E01E02.1080p.WEB.H264-GRP"),
            [reading("Severance", None, "s2e1"), reading("Severance", None, "s2e2")]
        );
        assert_eq!(spans("Show.S01E01-E02.720p"), [reading("Show", None, "s1e1"), reading("Show", None, "s1e2")]);
        assert_eq!(spans("Show.S01E01-02.720p"), [reading("Show", None, "s1e1"), reading("Show", None, "s1e2")]);
        assert_eq!(spans("Show.S01E02-GRP"), [reading("Show", None, "s1e2")]);
        let held = |path, torrent| episodes(path, torrent).iter().map(|e| (e.season, e.episode)).collect::<Vec<_>>();
        assert_eq!(held("Show.S01E01E02.1080p.mkv", ""), [(1, 1), (1, 2)]);
        assert_eq!(held("Season 2/E07 - The One.mkv", "Show Name (2019)"), [(2, 7)]);
        assert_eq!(held("video.mkv", "Show.Name.S01E02.720p.HDTV"), [(1, 2)]);
        assert_eq!(held("Movie.2024.1080p.mkv", ""), []);
    }

    fn film_named(name: &str, year: u16, namesakes: Namesakes) -> Wanted {
        Wanted { name: name.into(), year: Some(year), episode: None, namesakes, runtime: None, season_episodes: None }
    }

    fn episode_of(name: &str, year: u16, namesakes: Namesakes, season: u32, episode: u32) -> Wanted {
        Wanted { episode: Some(Episode { season, episode }), ..film_named(name, year, namesakes) }
    }

    fn fits<'a>(wanted: &Wanted, releases: &[&'a str]) -> Vec<(&'a str, Option<String>)> {
        releases.iter().map(|&release| (release, wanted.fit(release).map(|n| n.span.to_string()))).collect()
    }

    fn expect<'a>(releases: &[&'a str], spans: &[Option<&str>]) -> Vec<(&'a str, Option<String>)> {
        releases.iter().copied().zip(spans.iter().map(|span| span.map(String::from))).collect()
    }

    #[test]
    fn a_remake_fits_only_by_its_own_year() {
        let dune = [
            "Dune.2021.2160p.UHD.BluRay.x265.10bit.HDR.TrueHD.7.1.Atmos-DON",
            "Dune.1984.Extended.1080p.BluRay.x264-SADPANDA",
            "Dune.1080p.WEB-DL.DD5.1.H264",
            "Dune.Part.Two.2024.1080p.WEBRip.x264",
        ];
        let newer = film_named("Dune", 2021, Namesakes::Leading);
        let older = film_named("Dune", 1984, Namesakes::Shadowed);
        assert_eq!(fits(&newer, &dune), expect(&dune, &[Some("film"), None, Some("film"), None]));
        assert_eq!(fits(&older, &dune), expect(&dune, &[None, Some("film"), None, None]));

        let dead = [
            "Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP",
            "Night of the Living Dead (1990) 1080p BluRay",
            "Night Of The Living Dead ( 1968) English FULL HD",
            "night_of_the_living_dead",
            "Night.of.the.Living.Dead.1969.720p",
            "Living Dead Night Collection",
        ];
        let original = film_named("Night of the Living Dead", 1968, Namesakes::Leading);
        let remake = film_named("Night of the Living Dead", 1990, Namesakes::Shadowed);
        assert_eq!(
            fits(&original, &dead),
            expect(&dead, &[Some("film"), None, Some("film"), Some("film"), Some("film"), None])
        );
        assert_eq!(fits(&remake, &dead), expect(&dead, &[None, Some("film"), None, None, None, None]));

        let office = [
            "The.Office.S02E01.720p.HDTV.x264",
            "The.Office.2005.S02.1080p.BluRay.x264",
            "The.Office.2001.S02E01.DVDRip.XviD",
            "The Office (UK) S02 Complete",
            "The.Office.US.S02E01.720p.WEB-DL.DD5.1.H.264-CtrlHD",
        ];
        let american = episode_of("The Office", 2005, Namesakes::Leading, 2, 1);
        let british = episode_of("The Office", 2001, Namesakes::Shadowed, 2, 1);
        assert_eq!(fits(&american, &office), expect(&office, &[Some("s2e1"), Some("s2"), None, None, None]));
        assert_eq!(fits(&british, &office), expect(&office, &[None, None, Some("s2e1"), None, None]));
    }

    #[test]
    fn a_release_without_a_year_fits_only_the_title_listed_first() {
        let archive = "night_of_the_living_dead";
        let leading = film_named("Night of the Living Dead", 1968, Namesakes::Leading);
        let alone = film_named("Night of the Living Dead", 1968, Namesakes::None);
        let shadowed = film_named("Night of the Living Dead", 1990, Namesakes::Shadowed);
        let read = [&leading, &alone, &shadowed].map(|wanted| wanted.fit(archive));
        let yearless = Named { name: "night of the living dead".into(), year: None, span: Span::Film };
        assert_eq!(read, [Some(yearless.clone()), Some(yearless), None]);
        let dated = leading.fit("Night.of.the.Living.Dead.1968.1080p").unwrap();
        assert_eq!((dated.year, dated.span), (Some(1968), Span::Film));
    }

    #[test]
    fn a_year_in_the_title_is_part_of_the_name() {
        let releases = [
            "Blade.Runner.2049.2017.2160p.UHD.BluRay.x265",
            "Blade.Runner.2049.1080p.BluRay.mkv",
            "Blade.Runner.1982.Final.Cut.1080p.BluRay",
        ];
        let sequel = film_named("Blade Runner 2049", 2017, Namesakes::None);
        let original = film_named("Blade Runner", 1982, Namesakes::None);
        assert_eq!(fits(&sequel, &releases), expect(&releases, &[Some("film"), Some("film"), None]));
        assert_eq!(fits(&original, &releases), expect(&releases, &[None, None, Some("film")]));
    }

    #[test]
    fn an_episode_fits_itself_and_every_pack_that_holds_it() {
        let releases = [
            "Breaking.Bad.S03E05.720p.HDTV.x264-IMMERSE",
            "Breaking.Bad.2008.S03E05.1080p.WEB-DL",
            "Breaking.Bad.S03.1080p.BluRay.x264-ROVERS",
            "Breaking Bad Season 3 Complete 720p",
            "Breaking.Bad.S01-S05.1080p.BluRay.x265",
            "Breaking Bad (2008) Season 1-5 S01-S05 Complete 1080p",
            "Breaking.Bad.The.Complete.Series.720p.BluRay",
            "Breaking.Bad.S03E06.720p.HDTV.x264",
            "Breaking.Bad.S02.1080p.BluRay.x264",
            "Breaking Bad S01-S02 1080p",
            "Breaking.Bad.2013.S03E05.720p",
            "Breaking.Bad.Movie.2019.1080p",
        ];
        let wanted = episode_of("Breaking Bad", 2008, Namesakes::None, 3, 5);
        assert_eq!(
            fits(&wanted, &releases),
            expect(
                &releases,
                &[
                    Some("s3e5"),
                    Some("s3e5"),
                    Some("s3"),
                    Some("s3"),
                    Some("s1-5"),
                    Some("s1-5"),
                    Some("complete"),
                    None,
                    None,
                    None,
                    None,
                    None,
                ]
            )
        );
        let pilot = episode_of("Severance", 2022, Namesakes::None, 2, 2);
        let double = ["Severance.S02E01E02.1080p.WEB.H264-GRP", "Severance.S02E01-02.1080p", "Severance.S02E01.1080p"];
        assert_eq!(fits(&pilot, &double), expect(&double, &[Some("s2e2"), Some("s2e2"), None]));
    }

    #[test]
    fn a_fansub_number_is_an_episode_of_season_one() {
        let releases = [
            "[SubsPlease] Sousou no Frieren - 05 (1080p) [ABCD1234].mkv",
            "[Erai-raws] Sousou no Frieren S1 - 05v2 [720p]",
            "[SubsPlease] Sousou no Frieren - 06 (1080p) [ABCD1234].mkv",
            "[SubsPlease] Sousou no Frieren (01-28) (1080p) [Batch]",
        ];
        let wanted = episode_of("Sousou no Frieren", 2023, Namesakes::None, 1, 5);
        assert_eq!(fits(&wanted, &releases), expect(&releases, &[Some("s1e5"), Some("s1e5"), None, None]));
    }

    #[test]
    fn a_release_counts_the_words_beyond_its_title() {
        let night = film_named("Night of the Living Dead", 1968, Namesakes::Leading);
        let cases = [
            ("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", 0),
            ("Night of the Living Dead (1968) 1080p BRRip x264 -YTS", 0),
            ("Night of the Living Dead 1968 1080p MAX WEB-DL DDP2 0 H 264-GPRS", 1),
            ("Night of the Living Dead (1968) 4K UHD Blu-ray Remux", 0),
            ("Night Of The Living Dead ( 1968) English FULL HD", 1),
            ("Night Of The Living Dead (1968) Colorized", 1),
            ("Night of the Living Dead (1968) [colorised, 4k, 60FPS]", 1),
            ("Night Of The Living Dead 1968 Svengoolie", 1),
            ("Night Of The Living Dead (1968) Synthwave Horror Cut", 3),
            ("Night Of The Living Dead ( 1968) Doblada Al Español FULL HD", 3),
            ("Night of the Living Dead (1968) Super 8 Transfer Lukassen Transfer", 4),
        ];
        let counted = cases.map(|(release, _)| (release, night.other_words(release)));
        assert_eq!(counted, cases);
        let pioneer = episode_of("Pioneer One", 2010, Namesakes::None, 1, 1);
        let cases = [
            ("Pioneer.One.S01E01.720p.x264-VODO", 0),
            ("Pioneer One Season One Complete XviD", 0),
            ("Pioneer One S01E01 [720p HD]", 0),
            ("Pioneer One S01E01 720p x264 VODO", 1),
        ];
        assert_eq!(cases.map(|(release, _)| (release, pioneer.other_words(release))), cases);
        let sintel = film_named("Sintel", 2010, Namesakes::None);
        assert_eq!(sintel.other_words("Sintel.2010.1080p.WEB.H264-CLASSiCALHD"), 0);
        let greys = episode_of("Grey's Anatomy", 2005, Namesakes::None, 12, 1);
        assert_eq!(greys.other_words("Grey's Anatomy Season Twelve 720p"), 1);
    }

    #[test]
    fn a_wanted_title_searches_for_what_its_releases_are_named() {
        let night = |year, namesakes| film_named("Night of the Living Dead", year, namesakes).queries();
        assert_eq!(night(1968, Namesakes::Leading), ["Night of the Living Dead 1968", "Night of the Living Dead"]);
        assert_eq!(night(1968, Namesakes::None), ["Night of the Living Dead 1968", "Night of the Living Dead"]);
        assert_eq!(night(1990, Namesakes::Shadowed), ["Night of the Living Dead 1990"]);
        assert_eq!(
            episode_of("Breaking Bad", 2008, Namesakes::None, 3, 5).queries(),
            ["Breaking Bad S03E05", "Breaking Bad S03", "Breaking Bad complete"]
        );
        let unknown = Wanted { year: None, ..film_named("Sintel", 2010, Namesakes::None) };
        assert_eq!(unknown.queries(), ["Sintel"]);
    }

    #[test]
    fn a_wanted_title_writes_its_runtime_in_seconds() {
        let wanted = Wanted {
            runtime: Some(std::time::Duration::from_secs(47 * 60)),
            season_episodes: Some(13),
            ..episode_of("Breaking Bad", 2008, Namesakes::None, 3, 5)
        };
        let json = serde_json::to_string(&wanted).unwrap();
        assert_eq!(
            json,
            r#"{"name":"Breaking Bad","year":2008,"episode":{"season":3,"episode":5},"namesakes":"none","runtime_s":2820,"season_episodes":13}"#
        );
        assert_eq!(serde_json::from_str::<Wanted>(&json).unwrap(), wanted);
    }

    #[test]
    fn titles_compare_without_case_accents_or_punctuation() {
        let cases = [
            ("Spider-Man: No Way Home", "spider man no way home"),
            ("Spider-Man No Way Home", "spider man no way home"),
            ("Amélie", "amelie"),
            ("S.W.A.T.", "s w a t"),
            ("Mr. Robot", "mr robot"),
            ("Fast & Furious", "fast and furious"),
            ("Ocean's Eleven", "oceans eleven"),
            ("  WALL·E ", "wall e"),
        ];
        for (title, expected) in cases {
            assert_eq!(normalized(title), expected);
        }
    }
}
