use torrential_core::protocol::{Change, Event, FilesReply, GetReply};
use torrential_core::{Error, Machine, Preferences, SearchQuery, SearchResult};

use crate::state::Edit;

/// What the screen asks of the engine. Every method blocks; the TUI calls each one on its own
/// thread, so an implementation can hold one connection per call.
pub trait Backend: Send + Sync + 'static {
    /// Runs a search. `partial` receives the results so far and the sources still pending, each
    /// time a source answers.
    fn search(
        &self,
        query: SearchQuery,
        partial: &mut dyn FnMut(SearchResult, Vec<String>),
    ) -> Result<SearchResult, Error>;
    /// The files of a search result or a download. A download's files carry their progress.
    fn files(&self, reference: &str) -> Result<FilesReply, Error>;
    /// Starts downloading a search result. `files` holds exact paths; `None` means every file.
    fn get(&self, reference: &str, files: Option<Vec<String>>) -> Result<GetReply, Error>;
    /// Streams the video of a search result or a download, with its subtitles, and opens it in a
    /// player at once. Returns the player's name.
    fn play(&self, reference: &str) -> Result<String, Error>;
    fn pause(&self, id: &str) -> Result<Vec<Change>, Error>;
    fn resume(&self, id: &str) -> Result<Vec<Change>, Error>;
    /// Stops tracking a download. Its files stay on disk.
    fn remove(&self, id: &str) -> Result<Vec<Change>, Error>;
    fn verify(&self, id: &str) -> Result<Vec<Change>, Error>;
    /// Streams every download's events, starting with `Event::Snapshot`, until the engine goes away.
    fn watch(&self, event: &mut dyn FnMut(Event)) -> Result<(), Error>;
    /// Free bytes in the default download folder.
    fn free_space(&self) -> Option<u64>;
    /// The overrides saved in config.yaml.
    fn preferences(&self) -> Result<Preferences, Error>;
    /// This machine, detected by the engine's code, so the ranking panel can show it before any
    /// search.
    fn machine(&self) -> Option<Machine>;
    /// Applies `edit` to the overrides in config.yaml, read fresh, and saves the result, which it
    /// returns. Later searches rank with it, and downloads take their size cap from it.
    fn save_preferences(&self, edit: Edit) -> Result<Preferences, Error>;
    /// A newer torrential that the engine's last check found, unless updates are off.
    fn newer_version(&self) -> Option<String>;
    /// Installs the latest torrential over this binary and returns its version, or `None` when
    /// this binary is already the latest.
    fn upgrade(&self) -> Result<Option<String>, Error>;
}
