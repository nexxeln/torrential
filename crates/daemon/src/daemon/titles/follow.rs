//! Follows a player that `torrential play` opened, through the JSON socket mpv and IINA open,
//! and keeps where it is in the watch history as a browser tab or a TV keeps theirs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use torrential_core::time::now;
use torrential_core::watching::SAVE_EVERY;
use torrential_core::{ReleaseId, VideoRef, Watched};

use super::watching::{Screen, Watching};
use crate::log::log;

/// How long a player may take to open its socket once `torrential play` starts it.
const OPEN_WITHIN: Duration = Duration::from_secs(30);
/// How often to look for the socket meanwhile.
const LOOK_EVERY: Duration = Duration::from_millis(100);
/// What the player reports each time it changes, by the number it reports it under.
const OBSERVED: [&str; 3] = ["time-pos", "duration", "pause"];

/// Numbers the players followed, so each is a screen of its own.
static PLAYERS: AtomicU64 = AtomicU64::new(0);

/// Follows the player at `socket` playing `video` until the player quits, keeping its place.
pub fn follow(watching: Arc<Watching>, video: VideoRef, release: Option<ReleaseId>, socket: PathBuf) {
    let screen = Screen::Player(PLAYERS.fetch_add(1, Ordering::Relaxed));
    tokio::spawn(async move {
        let Some(stream) = open(&socket).await else {
            log(&format!("the player of {video} opened no socket at {}, so its place is not kept", socket.display()));
            return;
        };
        watching.start(&screen, &video);
        let player = Follower { watching: &watching, screen: &screen, video: &video, release };
        if let Err(error) = player.follow(stream).await {
            log(&format!("lost the player of {video}: {error}"));
        }
        watching.stop(&screen, &video);
        let _ = std::fs::remove_file(&socket);
    });
}

/// The socket at `path` once the player opens it, or `None` if it never does.
async fn open(path: &Path) -> Option<impl AsyncRead + AsyncWrite + use<>> {
    let deadline = Instant::now() + OPEN_WITHIN;
    loop {
        if let Ok(stream) = connect(path).await {
            return Some(stream);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(LOOK_EVERY).await;
    }
}

#[cfg(unix)]
async fn connect(path: &Path) -> std::io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path).await
}

/// mpv on Windows opens a named pipe where it opens a socket elsewhere.
#[cfg(windows)]
async fn connect(path: &Path) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tokio::net::windows::named_pipe::ClientOptions::new().open(path)
}

struct Follower<'a> {
    watching: &'a Watching,
    screen: &'a Screen,
    video: &'a VideoRef,
    release: Option<ReleaseId>,
}

/// What the player last said.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct Place {
    /// Seconds. A player that stops playing the video says nothing, so this stays where it was.
    position: Option<f64>,
    duration: Option<f64>,
    paused: bool,
}

impl Place {
    /// Takes in one line from the player. Answers true when the player resumes.
    fn heard(&mut self, line: &str) -> bool {
        let Ok(message) = serde_json::from_str::<Value>(line) else { return false };
        if message["event"] != "property-change" {
            return false;
        }
        let seconds = message["data"].as_f64().filter(|at| at.is_finite() && *at >= 0.0);
        match message["name"].as_str() {
            Some("time-pos") => self.position = seconds.or(self.position),
            Some("duration") => self.duration = seconds.filter(|length| *length > 0.0).or(self.duration),
            Some("pause") => {
                let paused = message["data"].as_bool().unwrap_or(self.paused);
                let resumed = self.paused && !paused;
                self.paused = paused;
                return resumed;
            }
            _ => {}
        }
        false
    }
}

impl Follower<'_> {
    /// Saves the place every few seconds, and once more when the player quits.
    async fn follow(&self, stream: impl AsyncRead + AsyncWrite) -> std::io::Result<()> {
        let (read, mut write) = tokio::io::split(stream);
        for (id, name) in OBSERVED.iter().enumerate() {
            let command = serde_json::json!({ "command": ["observe_property", id + 1, name] });
            write.write_all(format!("{command}\n").as_bytes()).await?;
        }
        let mut lines = BufReader::new(read).lines();
        let mut place = Place::default();
        let mut saved = place;
        let mut tick = tokio::time::interval(SAVE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let quit = loop {
            tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        if place.heard(&line) {
                            self.watching.start(self.screen, self.video);
                        }
                    }
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                },
                _ = tick.tick() => {
                    if place != saved {
                        self.save(place);
                        saved = place;
                    }
                }
            }
        };
        if place != saved {
            self.save(place);
        }
        quit
    }

    fn save(&self, place: Place) {
        let Some(position) = place.position else { return };
        let watched = Watched {
            video: self.video.clone(),
            position: place.duration.map_or(position, |duration| position.min(duration)),
            duration: place.duration,
            release: self.release,
            at: now(),
        };
        if let Err(error) = self.watching.save(self.screen, &watched) {
            log(&format!("cannot keep where {} stopped: {error}", self.video));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_place_follows_what_the_player_says_and_keeps_it_when_the_player_lets_go() {
        let mut place = Place::default();
        let said = |name: &str, data: Value| {
            serde_json::json!({ "event": "property-change", "id": 1, "name": name, "data": data }).to_string()
        };
        assert!(!place.heard(&said("duration", 5760.0.into())));
        assert!(!place.heard(&said("time-pos", 600.5.into())));
        assert!(!place.heard(&said("pause", true.into())));
        assert!(!place.heard(r#"{"request_id":0,"error":"success"}"#));
        assert!(!place.heard("not json"));
        assert_eq!(place, Place { position: Some(600.5), duration: Some(5760.0), paused: true });
        assert!(place.heard(&said("pause", false.into())), "playing after a pause resumes");
        assert!(!place.heard(&said("time-pos", Value::Null)));
        assert!(!place.heard(&said("time-pos", (-1.0).into())));
        assert_eq!(place.position, Some(600.5), "a player closing the video keeps its last place");
    }
}
