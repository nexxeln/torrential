use torrential_core::{Category, Resolution};

use crate::terms::Terms;

const CAM_ANY_CASE: &[&str] = &["hdcam", "camrip", "hdcamrip", "telesync", "telecine", "hdts", "hdtc"];
const CAM_AFTER_YEAR: &[&str] = &["CAM", "TS"];

const ADULT: &[&str] = &[
    "bangbros",
    "brazzers",
    "camwhores",
    "chaturbate",
    "digitalplayground",
    "evilangel",
    "fansly",
    "javhd",
    "manyvids",
    "myfreecams",
    "naughtyamerica",
    "onlyfans",
    "porn",
    "pornhub",
    "realitykings",
    "xnxx",
    "xvideos",
];

pub(crate) fn is_cam(name: &str, category: Option<Category>) -> bool {
    if category.is_some_and(|c| c != Category::Video) {
        return false;
    }
    let tokens: Vec<&str> = name.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
    let mut after_year = false;
    let mut previous = "";
    for token in tokens {
        if CAM_ANY_CASE.contains(&token.to_ascii_lowercase().as_str()) {
            return true;
        }
        if after_year && CAM_AFTER_YEAR.contains(&token) && !previous.eq_ignore_ascii_case("mpeg") {
            return true;
        }
        after_year |= token.len() == 4 && token.parse::<u16>().is_ok_and(|y| (1900..=2099).contains(&y));
        previous = token;
    }
    false
}

/// What a name calls a piece of a film, or something made around it, as a run of words.
const EXTRAS: [&[&str]; 16] = [
    &["trailer"],
    &["trailers"],
    &["teaser"],
    &["teasers"],
    &["promo"],
    &["clip"],
    &["clips"],
    &["snippet"],
    &["sample"],
    &["featurette"],
    &["featurettes"],
    &["preview"],
    &["soundtrack"],
    &["ost"],
    &["making", "of"],
    &["behind", "the", "scenes"],
];

/// Whether a name is a trailer, a clip, or another extra rather than the work itself. Words
/// the query holds do not count, so Trailer Park Boys is no trailer.
pub(crate) fn is_extra(name: &str, terms: &Terms) -> bool {
    let words: Vec<String> =
        name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let asked = |extra: &[&str]| extra.iter().all(|word| terms.words().iter().any(|own| own == word));
    EXTRAS
        .iter()
        .filter(|extra| !asked(extra))
        .any(|extra| words.windows(extra.len()).any(|run| run.iter().zip(*extra).all(|(word, wanted)| word == wanted)))
}

pub(crate) fn is_too_small(size: Option<u64>, resolution: Resolution) -> bool {
    let floor = match resolution {
        Resolution::Sd => 0,
        Resolution::P720 | Resolution::P1080 => 50_000_000,
        Resolution::P2160 => 500_000_000,
    };
    size.is_some_and(|size| size < floor)
}

pub(crate) fn is_adult(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric()).any(|t| ADULT.contains(&t.to_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_camera_releases() {
        assert!(is_cam("Oppenheimer 2023 HDCAM x264", None));
        assert!(is_cam("Oppenheimer.2023.HDTS.720p", Some(Category::Video)));
        assert!(is_cam("Some Movie 2024 TS 1080p", None));
        assert!(is_cam("Some Movie (2024) CAM", None));
        assert!(is_cam("Movie 2023 TELESYNC", None));
    }

    #[test]
    fn leaves_lookalikes_alone() {
        assert!(!is_cam("Cam Newton Highlights 2019", None));
        assert!(!is_cam("Dash CAM Compilation 2019", None));
        assert!(!is_cam("Concert 2021 MPEG-TS 1080p", None));
        assert!(!is_cam("Learn TS 2024 Edition", Some(Category::Book)));
        assert!(!is_cam("Webcam Recorder 2024 ts", None));
        assert!(!is_cam("ubuntu-24.04.5.1-desktop-amd64.iso", Some(Category::Software)));
    }

    #[test]
    fn spots_trailers_the_query_does_not_ask_for() {
        let film = Terms::parse("night of the living dead 1968");
        assert!(is_extra("Night Of The Living Dead (1968) Trailer HD 1080p", &film));
        assert!(is_extra("Night Of The Living Dead ( 1968) ORIGINAL TRAILER [ HD 1080p]", &film));
        assert!(is_extra("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.sample", &film));
        assert!(is_extra("Night of the Living Dead 1968 Behind the Scenes 720p", &film));
        assert!(is_extra("Night.of.the.Living.Dead.1968.Making.Of.DVDRip", &film));
        assert!(is_extra("Big Buck Bunny [10-second snippet]", &Terms::parse("big buck bunny 2008")));
        assert!(is_extra("Sintel OST 2010 FLAC", &Terms::parse("sintel 2010")));
        assert!(!is_extra("Night Of The Living Dead 720p 1968", &film));
        assert!(!is_extra("Night Of The Living Dead (1968) Colorized", &film));
        assert!(!is_extra("His Girl Friday (1940) Criterion + Extras (1080p", &Terms::parse("his girl friday 1940")));
        assert!(!is_extra("Trailer Park Boys S01 1080p", &Terms::parse("trailer park boys")));
        assert!(!is_extra("The Making of a Lady 2012 720p", &Terms::parse("the making of a lady 2012")));
    }

    #[test]
    fn spots_adult_names_without_eating_real_titles() {
        assert!(is_adult("Model OnlyFans Leak 2024"));
        assert!(is_adult("Brazzers.2024.1080p"));
        assert!(is_adult("Amateur Porn 2024"));
        assert!(!is_adult("xXx (2002) 1080p BluRay"));
        assert!(!is_adult("Sex Education S04 1080p"));
        assert!(!is_adult("Sussex Documentary 2021"));
        assert!(!is_adult("Portlandia S01 1080p"));
        assert!(!is_adult("Ubuntu 24.04 LTS"));
        assert!(!is_adult("[SubsPlease] Hentai Ouji to Warawanai Neko - 01 (1080p)"));
        assert!(!is_adult("HK Hentai Kamen 2013"));
    }
}
