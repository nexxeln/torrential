//! Subtitles from online sources, for videos whose torrent carries none.

use std::sync::{Arc, Mutex};

use torrential_core::Error;
use torrential_search::subtitles::{Found, Online, Subtitle, SubtitleConfig, Video};

use super::search_port::BoxFuture;

pub trait SubtitlePort: Send + Sync {
    /// Subtitles for `video`. `None` when no source answered, so asking again can do better.
    fn find<'a>(&'a self, video: &'a Video) -> BoxFuture<'a, Option<Found>>;
    /// The bytes of a subtitle `find` returned.
    fn fetch<'a>(&'a self, subtitle: &'a Subtitle) -> BoxFuture<'a, Result<Vec<u8>, Error>>;
}

pub struct Live(Online);

impl Live {
    pub fn new(config: &SubtitleConfig) -> Result<Self, Error> {
        Online::new(config).map(Self)
    }
}

impl SubtitlePort for Live {
    fn find<'a>(&'a self, video: &'a Video) -> BoxFuture<'a, Option<Found>> {
        Box::pin(self.0.find(video))
    }

    fn fetch<'a>(&'a self, subtitle: &'a Subtitle) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
        Box::pin(self.0.fetch(subtitle))
    }
}

/// How long the fake subtitles run, and how far apart their lines are, in seconds.
const FAKE_RUNTIME: u32 = 600;
const FAKE_EVERY: u32 = 4;

/// Answers without the network: every video with a readable name has the same three subtitles,
/// two English and one Spanish, whose lines say when they show.
#[derive(Clone, Default)]
pub struct Fake(Arc<Mutex<FakeState>>);

#[derive(Default)]
struct FakeState {
    silent: bool,
    asked: Vec<Video>,
    fetched: Vec<String>,
}

impl Fake {
    /// Makes every search fail, as when the sources are down.
    #[cfg(test)]
    pub fn silence(&self, silent: bool) {
        self.0.lock().unwrap().silent = silent;
    }

    /// The videos searched for, in order.
    #[cfg(test)]
    pub fn asked(&self) -> Vec<Video> {
        self.0.lock().unwrap().asked.clone()
    }

    /// The IDs of the subtitles downloaded, in order.
    #[cfg(test)]
    pub fn fetched(&self) -> Vec<String> {
        self.0.lock().unwrap().fetched.clone()
    }
}

impl SubtitlePort for Fake {
    fn find<'a>(&'a self, video: &'a Video) -> BoxFuture<'a, Option<Found>> {
        let mut state = self.0.lock().unwrap();
        state.asked.push(video.clone());
        let silent = state.silent;
        Box::pin(async move {
            if silent {
                return None;
            }
            let Some(title) = video.titles.first() else { return Some(Found::default()) };
            let stem = video.file_name.rsplit_once('.').map_or(video.file_name.as_str(), |(stem, _)| stem);
            let listed = |id: &str, language: &str, name: String, exact| Subtitle {
                id: id.into(),
                language: language.into(),
                name: Some(name),
                exact,
                source: "fake",
                url: format!("fake:{language}"),
            };
            let subtitles = vec![
                listed("fake-eng-1", "eng", format!("{stem}.en.srt"), video.hash.is_some()),
                listed("fake-eng-2", "eng", "Another.Release.en.srt".into(), false),
                listed("fake-spa-1", "spa", format!("{stem}.es.srt"), false),
            ];
            Some(Found { title: Some(title.clone()), subtitles })
        })
    }

    fn fetch<'a>(&'a self, subtitle: &'a Subtitle) -> BoxFuture<'a, Result<Vec<u8>, Error>> {
        self.0.lock().unwrap().fetched.push(subtitle.id.clone());
        Box::pin(async move {
            let says = if subtitle.language == "spa" { "Subtítulo de prueba a los" } else { "Test subtitle at" };
            let clock = |seconds: u32| format!("00:{:02}:{:02}", seconds / 60, seconds % 60);
            let cues = (0..FAKE_RUNTIME / FAKE_EVERY).map(|n| {
                let start = n * FAKE_EVERY + 1;
                format!(
                    "{}\r\n{},000 --> {},500\r\n{says} {}\r\n\r\n",
                    n + 1,
                    clock(start),
                    clock(start + 2),
                    clock(start)
                )
            });
            Ok(cues.collect::<String>().into_bytes())
        })
    }
}
