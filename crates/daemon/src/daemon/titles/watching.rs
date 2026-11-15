//! Where a person stopped each film and episode, and which screen keeps that place. A browser
//! tab, a TV, a title play and `torrential play` all read and save it here.
//!
//! The last screen to start or resume a video keeps its place. Another screen that plays it on
//! saves nothing until it starts or resumes again, or until the keeper stops.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use torrential_core::watching::WORTH_SAVING;
use torrential_core::{Error, VideoRef, Watched};

use super::super::shelf::Shelf;

/// A screen that plays a video and saves where it is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Screen {
    /// A browser tab, by the word it made up for the video it plays.
    Tab(String),
    /// A cast to a TV, by its number.
    Cast(u64),
    /// A player `torrential play` opened, by its number.
    Player(u64),
}

pub struct Watching {
    shelf: Arc<Shelf>,
    /// The screen that keeps each video's place.
    keepers: Mutex<HashMap<VideoRef, Screen>>,
}

impl Watching {
    pub fn new(shelf: Arc<Shelf>) -> Self {
        Self { shelf, keepers: Mutex::default() }
    }

    fn keepers(&self) -> MutexGuard<'_, HashMap<VideoRef, Screen>> {
        self.keepers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the person stopped `video`, unless they finished it or stopped too near the start.
    pub fn resume_at(&self, video: &VideoRef) -> Result<Option<f64>, Error> {
        Ok(torrential_core::watching::resume_at(&self.shelf.watched(&video.title)?, video))
    }

    /// `screen` starts or resumes `video`, so it keeps the place from now on.
    pub fn start(&self, screen: &Screen, video: &VideoRef) {
        self.keepers().insert(video.clone(), screen.clone());
    }

    /// `screen` no longer plays `video`. Of the screens still playing it, the next to save keeps
    /// the place.
    pub fn stop(&self, screen: &Screen, video: &VideoRef) {
        let mut keepers = self.keepers();
        if keepers.get(video) == Some(screen) {
            keepers.remove(video);
        }
    }

    /// Saves where `screen` is in the video if it keeps the place, and answers whether it does. A
    /// screen keeps it when it was the last to start or resume the video, or when no screen that
    /// did plays it any more. A place too near the start is not saved.
    pub fn save(&self, screen: &Screen, watched: &Watched) -> Result<bool, Error> {
        let kept = self.keepers().entry(watched.video.clone()).or_insert_with(|| screen.clone()) == screen;
        if kept && watched.position > WORTH_SAVING {
            self.shelf.watch(watched)?;
        }
        Ok(kept)
    }

    /// Forgets where the person stopped `video`, so it plays from the beginning.
    pub fn forget(&self, video: &VideoRef) -> Result<(), Error> {
        self.shelf.forget(video)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching(dir: &tempfile::TempDir) -> Watching {
        let db = crate::daemon::db::connect(&dir.path().join("state.db")).unwrap();
        Watching::new(Arc::new(Shelf::open(&db).unwrap()))
    }

    fn at(position: f64, at: i64) -> Watched {
        Watched { video: "tt0063350".parse().unwrap(), position, duration: Some(5760.0), release: None, at }
    }

    #[test]
    fn the_last_screen_to_start_or_resume_a_video_keeps_its_place() {
        let dir = tempfile::tempdir().unwrap();
        let watching = watching(&dir);
        let video: VideoRef = "tt0063350".parse().unwrap();
        let (tab, tv) = (Screen::Tab("a1".into()), Screen::Cast(1));
        let place = || watching.resume_at(&video).unwrap();

        watching.start(&tab, &video);
        assert!(watching.save(&tab, &at(100.0, 10)).unwrap());
        watching.start(&tv, &video);
        assert!(!watching.save(&tab, &at(110.0, 20)).unwrap(), "the TV started since");
        assert!(watching.save(&tv, &at(600.0, 30)).unwrap());
        assert_eq!(place(), Some(600.0));

        watching.start(&tab, &video);
        assert!(!watching.save(&tv, &at(605.0, 40)).unwrap(), "the tab resumed since");
        assert!(watching.save(&tab, &at(120.0, 50)).unwrap());
        assert_eq!(place(), Some(120.0));

        watching.stop(&tv, &video);
        assert!(watching.save(&tab, &at(130.0, 60)).unwrap(), "the TV stopping takes nothing");
        watching.stop(&tab, &video);
        assert!(watching.save(&tv, &at(700.0, 70)).unwrap(), "with no keeper, the next save keeps it");
        assert!(watching.save(&tv, &at(WORTH_SAVING, 80)).unwrap());
        assert_eq!(place(), Some(700.0), "a place at the start is not saved");

        watching.forget(&video).unwrap();
        assert_eq!(place(), None);
    }
}
