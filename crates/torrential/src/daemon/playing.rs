//! The streams the engine is sending to players now. An update waits until there are none, and
//! then lets none start until it restarts the engine, so a restart never cuts a film off.

use std::sync::Arc;

use tokio::sync::watch;
use torrential_core::{Error, ErrorCode};

#[derive(Clone, Copy, Default)]
struct Admission {
    open: usize,
    /// An update found nothing playing and is restarting the engine.
    updating: bool,
}

#[derive(Clone)]
pub struct Playing(Arc<watch::Sender<Admission>>);

/// One open stream, counted until it drops.
pub struct Open(Playing);

/// While this lives, no stream starts. Dropping it lets them start again.
pub struct Closed(Playing);

impl Playing {
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(Admission::default())))
    }

    /// Counts a stream, unless an update is restarting the engine.
    pub fn open(&self) -> Result<Open, Error> {
        let mut admitted = false;
        self.0.send_if_modified(|admission| {
            admitted = !admission.updating;
            admission.open += usize::from(admitted);
            admitted
        });
        if !admitted {
            return Err(Error::new(ErrorCode::DaemonUnavailable, "torrential is updating, try again in a few seconds"));
        }
        Ok(Open(self.clone()))
    }

    pub fn any(&self) -> bool {
        self.0.borrow().open > 0
    }

    /// Waits until no stream is open, then refuses new ones in the same step, so none starts
    /// between the last one ending and the restart.
    pub async fn close(&self) -> Closed {
        let mut changes = self.0.subscribe();
        loop {
            let closed = self.0.send_if_modified(|admission| {
                let quiet = admission.open == 0 && !admission.updating;
                admission.updating |= quiet;
                quiet
            });
            if closed {
                return Closed(self.clone());
            }
            let _ = changes.wait_for(|admission| admission.open == 0 && !admission.updating).await;
        }
    }
}

impl Closed {
    /// Keeps streams refused for as long as this process runs, which ends in the restart.
    pub fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.0.send_modify(|admission| admission.open -= 1);
    }
}

impl Drop for Closed {
    fn drop(&mut self) {
        self.0.0.send_modify(|admission| admission.updating = false);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use torrential_core::ErrorCode;

    use super::*;

    #[tokio::test]
    async fn the_wait_for_quiet_ends_when_the_last_stream_closes() {
        let playing = Playing::new();
        let (film, trailer) = (playing.open().unwrap(), playing.open().unwrap());
        let waiting = tokio::spawn({
            let playing = playing.clone();
            async move { playing.close().await }
        });
        drop(film);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "one stream is still open");
        drop(trailer);
        tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
        assert!(!playing.any());
    }

    #[tokio::test]
    async fn once_an_update_finds_nothing_playing_no_stream_starts_until_it_lets_go() {
        let playing = Playing::new();
        let film = playing.open().unwrap();
        let closing = tokio::spawn({
            let playing = playing.clone();
            async move { playing.close().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!closing.is_finished(), "the film still plays");
        let trailer = playing.open().unwrap();
        drop(film);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!closing.is_finished(), "the trailer started before the update closed playback");
        drop(trailer);
        let closed = tokio::time::timeout(Duration::from_secs(1), closing).await.unwrap().unwrap();

        let refused = playing.open().err().map(|e| (e.code, e.message));
        drop(closed);

        assert_eq!(
            refused,
            Some((ErrorCode::DaemonUnavailable, "torrential is updating, try again in a few seconds".to_string()))
        );
        assert!(playing.open().is_ok(), "an update that gave up lets streams start again");
    }
}
