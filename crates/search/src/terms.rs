use std::cmp::{Ordering, Reverse};
use std::collections::HashSet;

use torrential_core::name::{self, year};

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "at", "by", "de", "der", "die", "das", "el", "for", "in", "la", "le", "of", "on", "or", "the",
    "to", "with",
];

#[derive(Clone, Debug)]
pub(crate) struct Terms {
    text: String,
    words: Vec<String>,
    year: Option<u16>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum YearMatch {
    Other,
    Absent,
    Same,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Relevance {
    pub matched: u16,
    pub year: YearMatch,
}

impl Terms {
    pub fn parse(text: &str) -> Terms {
        let text = torrential_core::text::sanitize(text).to_lowercase();
        let mut words: Vec<String> = Vec::new();
        for word in tokens(&text) {
            if !words.contains(&word) {
                words.push(word);
            }
        }
        let year = words.iter().find_map(|w| year(w));
        Terms { text, words, year }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn words(&self) -> &[String] {
        &self.words
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn relevance(&self, name: &str) -> Relevance {
        let name_tokens = tokens(name);
        let matched = self.matches(name, &name_tokens).iter().filter(|hit| **hit).count();
        let year = match self.year {
            None => YearMatch::Absent,
            Some(wanted) => {
                let years: Vec<u16> = name_tokens.iter().filter_map(|t| year(t)).collect();
                if years.contains(&wanted) {
                    YearMatch::Same
                } else if years.is_empty() {
                    YearMatch::Absent
                } else {
                    YearMatch::Other
                }
            }
        };
        Relevance { matched: u16::try_from(matched).unwrap_or(u16::MAX), year }
    }

    pub fn extra_words(&self, name: &str) -> u16 {
        let name_tokens = tokens(name);
        let matched = self.matches(name, &name_tokens).iter().filter(|hit| **hit).count();
        u16::try_from(name_tokens.len().saturating_sub(matched)).unwrap_or(u16::MAX)
    }

    pub fn is_relevant(&self, name: &str) -> bool {
        let hits = self.matches(name, &tokens(name));
        let all_stopwords = self.words.iter().all(|w| STOPWORDS.contains(&w.as_str()));
        self.words.iter().zip(hits).any(|(word, hit)| hit && (all_stopwords || !STOPWORDS.contains(&word.as_str())))
    }

    pub fn covers(&self, name: &str) -> bool {
        !self.words.is_empty() && self.matches(name, &tokens(name)).iter().all(|hit| *hit)
    }

    fn matches(&self, name: &str, name_tokens: &[String]) -> Vec<bool> {
        let mut set: HashSet<String> = name_tokens.iter().cloned().collect();
        for run in 2..=3 {
            set.extend(name_tokens.windows(run).filter(|w| joinable(w)).map(|w| w.concat()));
        }
        let folded = name.to_lowercase();
        let mut hits: Vec<bool> = self
            .words
            .iter()
            .map(|w| if w.is_ascii() { set.contains(w) } else { folded.contains(w.as_str()) })
            .collect();
        for run in 2..=3 {
            for (start, window) in self.words.windows(run).enumerate() {
                if joinable(window) && set.contains(&window.concat()) {
                    hits[start..start + run].fill(true);
                }
            }
        }
        hits
    }
}

pub(crate) fn newer_first(a: &str, b: &str) -> Ordering {
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Word {
        Number(Reverse<u64>),
        Text(String),
    }
    let words = |name: &str| -> Vec<Word> {
        tokens(name).into_iter().map(|t| t.parse().map_or(Word::Text(t), |n| Word::Number(Reverse(n)))).collect()
    };
    words(a).cmp(&words(b))
}

fn joinable(run: &[String]) -> bool {
    let number = |t: &String| t.bytes().all(|b| b.is_ascii_digit());
    run.windows(2).all(|pair| !(number(&pair[0]) && number(&pair[1])))
}

fn tokens(s: &str) -> Vec<String> {
    name::words(s).map(str::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_words_and_joined_words() {
        let terms = Terms::parse("Arch Linux ISO");
        let r = terms.relevance("archlinux-2026.10.01-x86_64.iso");
        assert_eq!(r, Relevance { matched: 3, year: YearMatch::Absent });
        assert!(terms.covers("archlinux-2026.10.01-x86_64.iso"));
        assert!(!terms.covers("ubuntu-24.04.5.1-desktop-amd64.iso"));
    }

    #[test]
    fn ranks_the_query_year_above_a_missing_one_above_another() {
        let terms = Terms::parse("night of the living dead 1968");
        let same = terms.relevance("Night of the Living Dead (1968) 1080p");
        let absent = terms.relevance("Night of the Living Dead 1080p");
        let other = terms.relevance("Night of the Living Dead (1990) 1080p");
        assert_eq!(same, Relevance { matched: 6, year: YearMatch::Same });
        assert_eq!(absent, Relevance { matched: 5, year: YearMatch::Absent });
        assert_eq!(other, Relevance { matched: 5, year: YearMatch::Other });
        assert!(same > absent && absent > other);
    }

    #[test]
    fn counts_the_words_a_name_adds_to_the_query() {
        let terms = Terms::parse("debian 13.7 netinst");
        assert_eq!(terms.extra_words("debian-13.7.0-amd64-netinst.iso"), 3);
        assert_eq!(terms.extra_words("debian-edu-13.7.0-amd64-netinst.iso"), 4);
        assert_eq!(Terms::parse("night of the living dead 1968").extra_words("Night of the Living Dead (1968)"), 0);
    }

    #[test]
    fn newer_versions_and_dates_sort_first() {
        let mut names = [
            "Stack Exchange Data Dump (2026-06-30)",
            "debian-13.7.0-arm64-netinst.iso",
            "Stack Exchange Data Dump (2026-09-30)",
            "debian-13.10.0-amd64-netinst.iso",
            "DEBIAN-13.7.0-AMD64-netinst.iso",
        ];
        names.sort_by(|a, b| newer_first(a, b));
        assert_eq!(
            names,
            [
                "debian-13.10.0-amd64-netinst.iso",
                "DEBIAN-13.7.0-AMD64-netinst.iso",
                "debian-13.7.0-arm64-netinst.iso",
                "Stack Exchange Data Dump (2026-09-30)",
                "Stack Exchange Data Dump (2026-06-30)",
            ]
        );
    }

    #[test]
    fn joined_name_words_match_one_query_word() {
        let terms = Terms::parse("spiderman");
        assert!(terms.covers("Spider-Man No Way Home 2021"));
        assert_eq!(Terms::parse("wifi driver").relevance("Wi-Fi Driver Pack").matched, 2);
        assert!(!terms.covers("Spider Monkey Documentary"));
        assert!(Terms::parse("h264").covers("Movie 2024 1080p H.264"));
    }

    #[test]
    fn numbers_side_by_side_stay_apart() {
        let area51 = Terms::parse("area 51");
        assert!(!area51.is_relevant("Some Movie 2024 1080p BluRay DTS-HD MA 5.1 x264"));
        assert_eq!(area51.relevance("Area 52 2024 DD 5.1").matched, 1);
        assert_eq!(area51.relevance("Area 51 2015").matched, 2);
        assert!(!Terms::parse("cifar 10").covers("CIFAR-100 python version 1.0"));
    }

    #[test]
    fn stopwords_alone_do_not_make_a_row_relevant() {
        let terms = Terms::parse("the office");
        assert!(terms.is_relevant("The Office S01 720p"));
        assert!(!terms.is_relevant("The Matrix 1999"));
        assert!(Terms::parse("the").is_relevant("The Matrix 1999"));
    }

    #[test]
    fn non_latin_words_match_inside_longer_runs() {
        let terms = Terms::parse("進撃の巨人");
        assert!(terms.covers("[Group] 進撃の巨人 The Final Season 01"));
        assert_eq!(Terms::parse("  Ubuntu\t24.04 ").text(), "ubuntu 24.04");
    }
}
