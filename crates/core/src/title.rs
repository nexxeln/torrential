//! Reads which film or episode a video is from the names in its torrent.

/// A film, or with `episode` one episode of a series.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Title {
    /// The film's or the series' name as its words, without the dots and tags of a release name.
    pub name: String,
    /// The year in the name. For a series, the year it began.
    pub year: Option<u16>,
    pub episode: Option<Episode>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

    let season = file.season.or_else(|| around.iter().find_map(|r| r.season));
    // A bare number names an episode only next to a season, or in a fansub's `[Group] Title - 05`.
    let counted = file.counted.filter(|_| season.is_some() || file.grouped);
    if let Some(episode) = file.episode.or(counted) {
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
    let episode = |r: &Reading| Some(Episode { season: r.season.unwrap_or(1), episode: r.episode? });
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
    season: Option<u32>,
    episode: Option<u32>,
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
            let found = if let Some((season, episode)) = season_and_episode(token) {
                reading.season.get_or_insert(season);
                reading.episode.get_or_insert(episode);
                true
            } else if let Some(season) = letter_number(token, "s", 2).or(word_number(&lower, i, "season")) {
                reading.season.get_or_insert(season);
                true
            } else if let Some(episode) =
                letter_number(token, "ep", 3).or(letter_number(token, "e", 3)).or(word_number(&lower, i, "episode"))
            {
                reading.episode.get_or_insert(episode);
                true
            } else {
                let tag = TAGS.contains(&token.split('-').next().unwrap_or_default()) || TAGS.contains(&token.as_str());
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
                if at + 1 == end && reading.episode.is_none() && reading.season.is_none() {
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
/// dropped unless it holds a year. Also whether the name starts with a `[Group]`.
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
                    if c == ')' && year(held.trim()).is_some() {
                        spaced.push_str(held.trim());
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

/// `s01e02` and `1x02`, also at the start of `s01e02e03` and `s01e02-e03`.
fn season_and_episode(token: &str) -> Option<(u32, u32)> {
    /// The number of up to `most` digits that starts `text`, and what follows it.
    fn number(text: &str, most: usize) -> Option<(Option<u32>, &str)> {
        let length = text.bytes().take_while(u8::is_ascii_digit).count();
        (1..=most).contains(&length).then(|| (text[..length].parse().ok(), &text[length..]))
    }
    if let Some(rest) = token.strip_prefix('s') {
        let (season, rest) = number(rest, 2)?;
        let (episode, rest) = number(rest.strip_prefix('e')?, 3)?;
        return (rest.is_empty() || rest.starts_with(['e', '-'])).then_some((season?, episode?));
    }
    let (season, rest) = number(token, 2)?;
    let (episode, rest) = number(rest.strip_prefix('x')?, 3)?;
    (rest.is_empty() && token.len() >= 4).then_some((season?, episode?))
}

/// `s02` for the letter `s`, `e07` and `ep07` for `e` and `ep`.
fn letter_number(token: &str, letters: &str, most: usize) -> Option<u32> {
    digits(token.strip_prefix(letters)?, 1, most)
}

/// The number after the word at `i`, as in `Season 2`.
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
            ("Season 3/Episode 05.mkv", "Breaking Bad Complete", episode("Breaking Bad Complete", None, 3, 5)),
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
