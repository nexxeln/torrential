//! What every part of the engine reaches: the owner, the ports, and each feature's state.

use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};
use torrential_core::{Error, ErrorCode, Preferences};

use super::detector::Detector;
use super::engine_port::EngineHandle;
use super::metainfo::ParseTorrent;
use super::owner::{Command, Reply};
use super::search_port::SearchPort;

#[derive(Clone)]
pub struct OwnerHandle(pub mpsc::UnboundedSender<Command>);

impl OwnerHandle {
    pub async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, Error> {
        let (reply, answer) = oneshot::channel();
        self.0.send(make(reply)).map_err(|_| stopping())?;
        answer.await.map_err(|_| stopping())?
    }

    pub fn tell(&self, command: Command) {
        let _ = self.0.send(command);
    }
}

fn stopping() -> Error {
    Error::new(ErrorCode::DaemonUnavailable, "the engine is shutting down")
        .with_hint("run the command again in a moment")
}

pub struct Ctx {
    pub owner: OwnerHandle,
    pub search: Arc<dyn SearchPort>,
    pub machine: Arc<Detector>,
    pub engine: EngineHandle,
    pub parse: ParseTorrent,
    pub version: String,
    /// The preferences in config.yaml now, which rank searches and cap new downloads.
    pub preferences: Arc<dyn Fn() -> Preferences + Send + Sync>,
    /// Where `http` serves downloads to players, and the web app.
    pub http_port: u16,
    pub web: super::web::State,
    /// ffmpeg, and the tracks and thumbnails of the videos the engine has read.
    pub media: super::media::State,
    pub titles: super::titles::Titles,
    pub playing: super::playing::Playing,
    pub news: super::switch::Board,
    /// The phones and TVs this Mac let in, and the grants casts play from.
    pub devices: Arc<super::devices::Devices>,
    /// The TVs on the home network, and the casts playing on them.
    pub casts: super::cast::Casts,
    /// The Torznab indexers in config.yaml.
    pub indexers: super::indexers::Indexers,
}
