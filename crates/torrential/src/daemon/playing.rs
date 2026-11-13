//! The streams the engine is sending to players now. An update waits until there are none, so a
//! restart never cuts a film off.

use std::sync::Arc;

use tokio::sync::watch;

#[derive(Clone)]
pub struct Playing(Arc<watch::Sender<usize>>);

/// One open stream, counted until it drops.
pub struct Open(Playing);

impl Playing {
    pub fn new() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }

    pub fn open(&self) -> Open {
        self.0.send_modify(|count| *count += 1);
        Open(self.clone())
    }

    pub fn any(&self) -> bool {
        *self.0.borrow() > 0
    }

    /// Returns once no stream is open.
    pub async fn none(&self) {
        let _ = self.0.subscribe().wait_for(|count| *count == 0).await;
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.0.send_modify(|count| *count -= 1);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn the_wait_for_quiet_ends_when_the_last_stream_closes() {
        let playing = Playing::new();
        let (film, trailer) = (playing.open(), playing.open());
        let waiting = tokio::spawn({
            let playing = playing.clone();
            async move { playing.none().await }
        });
        drop(film);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "one stream is still open");
        drop(trailer);
        tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
        assert!(!playing.any());
    }
}
