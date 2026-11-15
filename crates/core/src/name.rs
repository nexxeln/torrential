//! How release names split into words and years. The title reader, quality, cut and architecture
//! tags, language markers and the search's relevance all split names this way.

/// A year as a name writes it: four digits from 1900 to 2099.
pub fn year(token: &str) -> Option<u16> {
    let year: u16 = (token.len() == 4 && token.bytes().all(|b| b.is_ascii_digit())).then(|| token.parse().ok())??;
    (1900..2100).contains(&year).then_some(year)
}

/// The words of a name: its runs of letters and digits, in any script.
pub fn words(name: &str) -> impl Iterator<Item = &str> {
    name.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty())
}

/// The tags of a name, in lower case: its runs of ASCII letters and digits. Tags are ASCII, so
/// any other character ends one.
pub fn tags(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|tag| !tag.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_splits_into_words_tags_and_years() {
        let name = "Amélie.2001.1080p_x264-[Grp]";
        assert_eq!(words(name).collect::<Vec<_>>(), ["Amélie", "2001", "1080p", "x264", "Grp"]);
        assert_eq!(tags(name), ["am", "lie", "2001", "1080p", "x264", "grp"]);
        assert_eq!(
            ["1899", "1900", "2099", "2100", "968", "+968", "19680"].map(year),
            [None, Some(1900), Some(2099), None, None, None, None]
        );
    }
}
