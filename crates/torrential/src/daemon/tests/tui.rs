//! Plays from the terminal UI, against the engine over its socket: what the player is handed.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use torrential_core::{VideoRef, Watched};
use torrential_tui::Backend;

use super::super::admission::GIB;
use super::super::search_port::{self, FakeSource};
use super::super::server;
use super::titles::{listed, night};
use super::{Harness, MIB, torrent};
use crate::client::tui::EngineBackend;
use crate::env::Env;
use crate::paths::Paths;

/// The terminal UI of bare `torrential`, connected to `h`'s socket. Its only player is a stub
/// `mpv` that writes its arguments to a file, so no real player ever starts.
struct Tui {
    backend: Arc<EngineBackend>,
    dir: tempfile::TempDir,
    port: u16,
}

impl Tui {
    fn new(h: &Harness) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        let mpv = dir.path().join("bin/mpv");
        std::fs::write(
            &mpv,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {home}/argv.part\nmv {home}/argv.part {home}/argv\n"),
        )
        .unwrap();
        std::fs::set_permissions(&mpv, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.path().join("th/config")).unwrap();
        std::fs::write(dir.path().join("th/config/config.yaml"), "player: mpv\n").unwrap();
        let vars = [
            ("HOME", home.clone()),
            ("TORRENTIAL_HOME", format!("{home}/th")),
            ("PATH", format!("{home}/bin")),
            ("TORRENTIAL_TEST_VERSION", h.running.ctx.version.clone()),
        ];
        let env = Env::from_pairs(vars.iter().map(|(key, value)| (*key, value.as_str())));
        let paths = Paths::from_process(&env).unwrap();
        let socket = paths.socket();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let ctx = h.running.ctx.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(server::connection(ctx.clone(), stream));
            }
        });
        let backend = Arc::new(EngineBackend { paths, downloads: h.downloads(), env });
        Self { backend, dir, port: h.running.ctx.http_port }
    }

    /// Plays `video` as Enter or P on a film, an episode, or a title's Play row does. Answers the
    /// player's name, where the engine said to resume, and the player's arguments, one a line,
    /// with the stream port as PORT.
    async fn play(&self, video: &str) -> (String, Option<f64>, String) {
        let backend = self.backend.clone();
        let video: VideoRef = video.parse().unwrap();
        let played = tokio::task::spawn_blocking(move || backend.play_title(&video, &mut |_| {}));
        let (player, played) = played.await.unwrap().unwrap();
        let argv = self.dir.path().join("argv");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !argv.exists() {
            assert!(Instant::now() < deadline, "the player starts");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let handed = std::fs::read_to_string(&argv).unwrap();
        std::fs::remove_file(&argv).unwrap();
        (player, played.resume_at, handed.replace(&format!("127.0.0.1:{}/", self.port), "127.0.0.1:PORT/"))
    }
}

fn watch(h: &Harness, video: &str, position: f64, duration: f64) {
    let watched = Watched { video: video.parse().unwrap(), position, duration: Some(duration), release: None, at: 1 };
    h.running.ctx.titles.shelf.watch(&watched).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_from_the_terminal_ui_starts_the_player_where_the_person_stopped() {
    let (h, _) = night();
    watch(&h, "tt0063350", 200.5, 5760.0);
    let a4 = "a4".repeat(20);
    assert_eq!(
        Tui::new(&h).play("tt0063350").await,
        (
            "mpv".to_string(),
            Some(200.5),
            format!(
                "--sub-file=http://127.0.0.1:PORT/{a4}/1/night_of_the_living_dead.srt\n\
                 --force-media-title=Night of the Living Dead (1968)\n--start=200\n\
                 http://127.0.0.1:PORT/{a4}/0/night_of_the_living_dead.mp4\n"
            )
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_play_from_the_terminal_ui_of_a_finished_film_or_a_caught_up_series_starts_at_the_beginning() {
    let (h, _) = night();
    watch(&h, "tt0063350", 5750.0, 5760.0);
    let a4 = "a4".repeat(20);
    assert_eq!(
        Tui::new(&h).play("tt0063350").await,
        (
            "mpv".to_string(),
            None,
            format!(
                "--sub-file=http://127.0.0.1:PORT/{a4}/1/night_of_the_living_dead.srt\n\
                 --force-media-title=Night of the Living Dead (1968)\n\
                 http://127.0.0.1:PORT/{a4}/0/night_of_the_living_dead.mp4\n"
            )
        )
    );

    let pack = torrent(
        0xb1,
        "Harbor.Lights.S01.1080p.WEB.x264",
        &[
            ("Harbor.Lights.S01E01.1080p.WEB.x264.mp4", 900 * MIB),
            ("Harbor.Lights.S01E02.1080p.WEB.x264.mp4", 900 * MIB),
            ("Harbor.Lights.S01E03.1080p.WEB.x264.mp4", 900 * MIB),
        ],
    );
    let search = search_port::Fake::scripted(
        vec![listed(&pack, "knaben", Some(20))],
        vec![FakeSource::answers("knaben", 0)],
        BTreeMap::new(),
    );
    let h = Harness::with_search(search);
    h.swarm.publish(pack);
    h.swarm.set_rate(GIB);
    watch(&h, "tt9999901:2:1", 2640.0, 2640.0);
    let b1 = "b1".repeat(20);
    assert_eq!(
        Tui::new(&h).play("tt9999901").await,
        (
            "mpv".to_string(),
            None,
            format!(
                "--force-media-title=Harbor Lights S01E01\n\
                 http://127.0.0.1:PORT/{b1}/0/Harbor.Lights.S01E01.1080p.WEB.x264.mp4\n"
            )
        ),
        "the series is caught up, since S02E02 has not aired, so Play starts it again"
    );
}
