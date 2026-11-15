//! Which cut of a film a release name says it is. Subtitles timed for one cut drift out of step
//! with another wherever their scenes differ, and no single offset fixes that.

use crate::name;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Cut {
    /// What cinemas showed. A name that says no cut is this one.
    #[default]
    Theatrical,
    Extended,
    DirectorsCut,
    FinalCut,
    Ultimate,
    Unrated,
    Uncut,
    SpecialEdition,
    Redux,
    Remastered,
}

/// The tags that name each cut. A name that says two is the one listed first: the cuts that
/// change the running time come before `remastered`, which keeps it.
const CUTS: [(&[&str], Cut); 16] = [
    (&["extended"], Cut::Extended),
    (&["directors", "cut"], Cut::DirectorsCut),
    (&["director", "s", "cut"], Cut::DirectorsCut),
    (&["director", "cut"], Cut::DirectorsCut),
    (&["dircut"], Cut::DirectorsCut),
    (&["dc"], Cut::DirectorsCut),
    (&["final", "cut"], Cut::FinalCut),
    (&["ultimate", "cut"], Cut::Ultimate),
    (&["ultimate", "edition"], Cut::Ultimate),
    (&["unrated"], Cut::Unrated),
    (&["uncut"], Cut::Uncut),
    (&["special", "edition"], Cut::SpecialEdition),
    (&["redux"], Cut::Redux),
    (&["theatrical"], Cut::Theatrical),
    (&["remastered"], Cut::Remastered),
    (&["remaster"], Cut::Remastered),
];

impl Cut {
    pub fn of(name: &str) -> Cut {
        let tags = name::tags(name);
        let says = |words: &[&str]| tags.windows(words.len()).any(|run| run == words);
        CUTS.iter().find(|(words, _)| says(words)).map_or(Cut::Theatrical, |&(_, cut)| cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_cut_from_recorded_names() {
        let cases = [
            ("Watchmen.extended .DVDRip.XviD", Cut::Extended),
            ("Watchmen.DC.DVDRip.XviD-HNR", Cut::DirectorsCut),
            ("Blade Runner : Director Cut", Cut::DirectorsCut),
            ("Blade Runner Director's Cut CD1.srt", Cut::DirectorsCut),
            ("Blade Runner Directors Cut (SPA).srt", Cut::DirectorsCut),
            ("Terminator 2: Judgment Day - Special Edition", Cut::SpecialEdition),
            ("Dune.1984.Extended.1080p.BluRay.x264-SADPANDA", Cut::Extended),
            ("The.Matrix.1999.REMASTERED.1080p.BluRay.x264-GROUP.mkv", Cut::Remastered),
            ("Night of the Living Dead 1968 4K Remaster", Cut::Remastered),
            ("Movie.2010.Remastered.Extended.1080p", Cut::Extended),
            ("Movie.2010.Theatrical.Remastered.1080p", Cut::Theatrical),
            ("Night Of The Living Dead (1968) Millenium Edition (Osloskop Net)", Cut::Theatrical),
            ("Watchmen.2009.DVDRip.XviD-JUMANJi", Cut::Theatrical),
            ("Sintel (2010)", Cut::Theatrical),
            ("", Cut::Theatrical),
        ];
        let read: Vec<(&str, Cut)> = cases.iter().map(|&(name, _)| (name, Cut::of(name))).collect();
        assert_eq!(read, cases);
    }
}
