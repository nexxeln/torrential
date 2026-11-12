use std::path::Path;
use std::time::Duration;

use crate::job::TorrentFile;
use crate::title::{self, Episode};

/// Folders that hold the subtitles for every video in the torrent.
const SUBTITLE_FOLDERS: [&str; 4] = ["sub", "subs", "subtitle", "subtitles"];

/// Plan for this share of the current rate, since a rate measured over a few seconds swings as peers come and go.
const MARGIN: f64 = 0.9;

/// Video on disk before playback starts.
const HEAD_BUFFER: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Playable {
    pub video: u32,
    /// Ordered by path.
    pub subtitles: Vec<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Start {
    Now,
    /// Whole seconds, rounded up.
    Wait(Duration),
    /// Bytes are missing and no peer is sending any.
    Stalled,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Video,
    Subtitle,
}

/// The largest video file, plus the subtitle files that belong with it.
pub fn pick(files: &[TorrentFile]) -> Option<Playable> {
    let videos: Vec<&TorrentFile> = files.iter().filter(|f| kind(&f.path) == Some(Kind::Video)).collect();
    let features: Vec<&TorrentFile> = videos.iter().copied().filter(|f| !is_sample(&f.path)).collect();
    let video = if features.is_empty() { &videos } else { &features }.iter().max_by_key(|f| f.size)?;
    let subtitles = subtitles(files, video, features.len());
    Some(Playable { video: video.index, subtitles: subtitles.into_iter().map(|f| f.index).collect() })
}

/// The file a person chose to watch, plus the subtitle files that belong with it. When some of
/// them carry the video's name, as each episode's do in a season, only those belong.
pub fn around(files: &[TorrentFile], video: u32) -> Option<Playable> {
    let video = files.iter().find(|f| f.index == video)?;
    let features = files.iter().filter(|f| kind(&f.path) == Some(Kind::Video) && !is_sample(&f.path)).count();
    let mut subtitles = subtitles(files, video, features);
    let name = video.path.rsplit('/').next().unwrap_or(&video.path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_lowercase();
    if subtitles.iter().any(|f| f.path.to_lowercase().contains(&stem)) {
        subtitles.retain(|f| f.path.to_lowercase().contains(&stem));
    }
    Some(Playable { video: video.index, subtitles: subtitles.into_iter().map(|f| f.index).collect() })
}

/// The file of `episode` in a pack, plus the subtitle files that belong with it: the largest
/// video, not a sample, whose own name, folder, or torrent reads as that episode.
pub fn episode(files: &[TorrentFile], torrent: &str, episode: Episode) -> Option<Playable> {
    let video = files
        .iter()
        .filter(|f| kind(&f.path) == Some(Kind::Video) && !is_sample(&f.path))
        .filter(|f| title::episodes(&f.path, torrent).contains(&episode))
        .max_by_key(|f| f.size)?;
    around(files, video.index)
}

/// The subtitle files that belong with `video`, ordered by path, in a torrent with `features` videos.
fn subtitles<'a>(files: &'a [TorrentFile], video: &TorrentFile, features: usize) -> Vec<&'a TorrentFile> {
    let home = folder(&video.path);
    let mut subtitles: Vec<&TorrentFile> = files
        .iter()
        .filter(|f| kind(&f.path) == Some(Kind::Subtitle))
        .filter(|f| features <= 1 || folder(&f.path) == home || in_subtitle_folder(&f.path))
        .collect();
    subtitles.sort_by(|a, b| a.path.cmp(&b.path));
    subtitles
}

/// When playback can start so it never catches up with the download.
pub fn start(size: u64, runtime: Duration, have: u64, rate: u64) -> Start {
    match (size.saturating_sub(have), rate) {
        (0, _) => Start::Now,
        (_, 0) => Start::Stalled,
        (missing, rate) => {
            let rate = rate as f64 * MARGIN;
            let head = (size as f64 * HEAD_BUFFER.as_secs_f64() / runtime.as_secs_f64()).min(size as f64);
            let wait = (missing as f64 / rate - runtime.as_secs_f64()).max((head - have as f64) / rate);
            if wait > 0.0 { Start::Wait(Duration::from_secs(wait.ceil() as u64)) } else { Start::Now }
        }
    }
}

fn kind(path: &str) -> Option<Kind> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "mkv" | "mp4" | "m4v" | "mov" | "webm" | "avi" | "ts" | "m2ts" | "wmv" | "mpg" | "mpeg" | "ogv" => {
            Some(Kind::Video)
        }
        "srt" | "ass" | "ssa" | "vtt" | "sub" => Some(Kind::Subtitle),
        _ => None,
    }
}

/// Whether the file at `path` is a sample clip, by its own name or a folder's.
pub fn is_sample(path: &str) -> bool {
    path.split('/').any(|part| part.to_ascii_lowercase().starts_with("sample"))
}

fn folder(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}

fn in_subtitle_folder(path: &str) -> bool {
    folder(path).split('/').any(|part| SUBTITLE_FOLDERS.contains(&part.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked<'a>(listing: &[(&'a str, u64)]) -> Option<(&'a str, Vec<&'a str>)> {
        let files: Vec<TorrentFile> = (0..)
            .zip(listing)
            .map(|(index, &(path, size))| TorrentFile {
                index,
                path: path.into(),
                size,
                selected: true,
                verified: 0,
                risk: None,
            })
            .collect();
        let playable = pick(&files)?;
        let path = |index: u32| listing[index as usize].0;
        Some((path(playable.video), playable.subtitles.into_iter().map(path).collect()))
    }

    #[test]
    fn picks_the_feature_and_its_subtitles() {
        let movie = [
            ("Movie.2024.1080p.mkv", 4_000_000_000),
            ("Sample/movie.2024.sample.mkv", 30_000_000),
            ("Subs/3_French.srt", 82_000),
            ("Subs/2_English.srt", 80_000),
            ("Forced/English.srt", 2_000),
            ("Movie.2024.1080p.nfo", 4_000),
        ];
        let season = [
            ("Show.S01E01.mkv", 1_000_000_000),
            ("Show.S01E02.mkv", 2_000_000_000),
            ("Show.S01E01.en.srt", 40_000),
            ("Show.S01E02.en.srt", 40_000),
            ("Show.S01E02.de.VTT", 40_000),
            ("Extras/Making Of.mkv", 500_000_000),
            ("Extras/Making Of.srt", 10_000),
            ("SUBTITLES/Show.S01E02.fr.ass", 50_000),
        ];
        let read = [
            picked(&movie),
            picked(&season),
            picked(&[("Sample.mkv", 900_000_000), ("Feature.MP4", 700_000_000)]),
            picked(&[("sample-clip.mkv", 19_000_000), ("notes.txt", 1_000)]),
            picked(&[("Film/Film.mp4", 700_000_000), ("Extras/Commentary.srt", 60_000), ("Film/Film.idx", 9_000)]),
            picked(&[("ubuntu-24.04.iso", 6_000_000_000), ("readme.srt", 1_000)]),
            picked(&[]),
        ];
        assert_eq!(
            read,
            [
                Some(("Movie.2024.1080p.mkv", vec!["Forced/English.srt", "Subs/2_English.srt", "Subs/3_French.srt"])),
                Some((
                    "Show.S01E02.mkv",
                    vec![
                        "SUBTITLES/Show.S01E02.fr.ass",
                        "Show.S01E01.en.srt",
                        "Show.S01E02.de.VTT",
                        "Show.S01E02.en.srt",
                    ]
                )),
                Some(("Feature.MP4", vec![])),
                Some(("sample-clip.mkv", vec![])),
                Some(("Film/Film.mp4", vec!["Extras/Commentary.srt"])),
                None,
                None,
            ]
        );
    }

    #[test]
    fn a_chosen_episode_takes_the_subtitles_that_carry_its_name() {
        let listing = [
            ("Show.S01E01.mkv", 1_000_000_000),
            ("Show.S01E02.mkv", 2_000_000_000),
            ("Show.S01E01.en.srt", 40_000),
            ("Show.S01E02.en.srt", 40_000),
            ("Subs/Show.S01E01/2_English.srt", 40_000),
            ("Extras/Making Of.mkv", 500_000_000),
            ("Extras/Making Of.srt", 10_000),
            ("Bonus/Clip.mkv", 100_000_000),
            ("Bonus/English.srt", 10_000),
        ];
        let files: Vec<TorrentFile> = (0..)
            .zip(listing)
            .map(|(index, (path, size))| TorrentFile {
                index,
                path: path.into(),
                size,
                selected: false,
                verified: 0,
                risk: None,
            })
            .collect();
        let chosen = |video| around(&files, video).map(|playable| (playable.video, playable.subtitles));
        assert_eq!(chosen(0), Some((0, vec![2, 4])));
        assert_eq!(chosen(1), Some((1, vec![3])));
        assert_eq!(chosen(5), Some((5, vec![6])));
        // No subtitle carries this name, so every one in its folder or a subtitle folder belongs.
        assert_eq!(chosen(7), Some((7, vec![8, 4])));
        assert_eq!(chosen(9), None);
    }

    #[test]
    fn an_episode_is_the_file_in_the_pack_that_holds_it() {
        let listed = |listing: &[(&str, u64)]| -> Vec<TorrentFile> {
            (0..)
                .zip(listing)
                .map(|(index, &(path, size))| TorrentFile {
                    index,
                    path: path.into(),
                    size,
                    selected: false,
                    verified: 0,
                    risk: None,
                })
                .collect()
        };
        let season = listed(&[
            ("Show.S01E01.mkv", 1_000_000_000),
            ("Show.S01E02.mkv", 2_000_000_000),
            ("Show.S01E01.en.srt", 40_000),
            ("Show.S01E02.en.srt", 40_000),
            ("Subs/Show.S01E01/2_English.srt", 40_000),
            ("Sample/Show.S01E03.sample.mkv", 30_000_000),
            ("Show.S01E04E05.mkv", 2_500_000_000),
            ("Extras/Making Of.mkv", 500_000_000),
        ]);
        let found = |files: &[TorrentFile], torrent: &str, season: u32, number: u32| {
            episode(files, torrent, Episode { season, episode: number }).map(|p| (p.video, p.subtitles))
        };
        let pack = "Show.S01.1080p.WEB-DL.x265";
        assert_eq!(found(&season, pack, 1, 1), Some((0, vec![2, 4])));
        assert_eq!(found(&season, pack, 1, 2), Some((1, vec![3])));
        assert_eq!(found(&season, pack, 1, 3), None, "only a sample holds it");
        assert_eq!(found(&season, pack, 1, 5), Some((6, vec![2, 3, 4])));
        assert_eq!(found(&season, pack, 2, 1), None);

        let series = listed(&[
            ("Show Name/Season 1/01 - Pilot.mkv", 900_000_000),
            ("Show Name/Season 2/01 - Return.mkv", 1_100_000_000),
            ("Show Name/Season 2/01 - Return.srt", 50_000),
        ]);
        assert_eq!(found(&series, "Show Name Complete", 2, 1), Some((1, vec![2])));
        assert_eq!(found(&series, "Show Name Complete", 1, 1), Some((0, vec![])));
        let single = listed(&[("bb305.mkv", 700_000_000), ("bb305.srt", 30_000)]);
        assert_eq!(found(&single, "Breaking.Bad.S03E05.720p.HDTV.x264-IMMERSE", 3, 5), Some((0, vec![1])));
    }

    #[test]
    fn waits_until_playback_cannot_catch_the_download() {
        let (size, runtime) = (5_000_000_000, Duration::from_secs(90 * 60));
        let read = [
            start(size, runtime, 0, 695_000),
            start(size, runtime, 0, 1_200_000),
            start(size, runtime, 0, 50_000_000),
            start(size, runtime, 10_000_000, 50_000_000),
            start(size, runtime, size, 0),
            start(size, runtime, 4_000_000_000, 0),
            start(3_107, Duration::from_secs(7), 0, 1_000),
        ];
        // The first is about 30 minutes at the full 5.56 Mbit/s, and 43 minutes when planning for 90% of it.
        assert_eq!(
            read,
            [
                Start::Wait(Duration::from_secs(2594)),
                Start::Wait(Duration::from_secs(9)),
                Start::Wait(Duration::from_secs(1)),
                Start::Now,
                Start::Now,
                Start::Stalled,
                Start::Wait(Duration::from_secs(4)),
            ]
        );
    }
}
