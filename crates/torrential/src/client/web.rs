//! `torrential web`: opens the web app the engine serves.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Serialize;
use torrential_core::{Error, ErrorCode};

use super::connect::Conn;
use super::emit;
use crate::cli::WebArgs;
use crate::daemon::web::ffmpeg::Tools;
use crate::env::Env;
use crate::output::theme::Token;
use crate::output::views::{Check, CheckStatus};
use crate::output::{Body, Cell, Doc, Mode, Typed};

#[derive(Serialize)]
struct Web {
    url: String,
    /// False when `--no-open` or `--json` kept the browser closed.
    opened: bool,
}

pub fn run(conn: &Conn, args: WebArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    if conn.http_port == 0 {
        return Err(Error::new(ErrorCode::DaemonVersion, "the running engine is older than the web app")
            .with_hint("run torrential engine stop, then run the command again")
            .with_next("torrential engine stop"));
    }
    let url = format!("http://127.0.0.1:{}", conn.http_port);
    let opened = !args.no_open && mode != Mode::Json;
    if opened {
        open(&url, env)?;
    }
    let state = if opened { "opened in your browser" } else { "ready" };
    let doc = Doc {
        body: Body::Fields {
            title: Vec::new(),
            fields: vec![("url", Cell::styled(url.clone(), Token::Accent)), ("state", Cell::new(state))],
        },
        summary: vec!["the web app works while the engine runs".to_string()],
        next: None,
    };
    Ok(emit(mode, &doc, &Typed { kind: "web", body: Web { url, opened } }))
}

/// Opens `url` in the default browser. Tests name a program in `TORRENTIAL_TEST_OPENER` to run in
/// its place.
fn open(url: &str, env: &Env) -> Result<(), Error> {
    let program = match env.var("TORRENTIAL_TEST_OPENER") {
        Some(program) => PathBuf::from(program),
        None if cfg!(target_os = "macos") => PathBuf::from("/usr/bin/open"),
        None => PathBuf::from("xdg-open"),
    };
    Command::new(&program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
        .map_err(|e| {
            Error::new(ErrorCode::NotFound, format!("cannot open a browser with {}: {e}", program.display()))
                .with_hint(format!("open {url} in your browser"))
        })
}

/// The `torrential doctor` line about ffmpeg, which the web player uses to play what the browser
/// cannot read on its own.
pub fn check(env: &Env) -> Check {
    let tools = Tools::find(env.var("PATH"));
    let (status, detail) = match (&tools.ffmpeg, &tools.ffprobe) {
        (Some(ffmpeg), Some(_)) => {
            (CheckStatus::Ok, format!("{}; the web player converts what the browser cannot read", ffmpeg.display()))
        }
        _ => (
            CheckStatus::Warn,
            "ffmpeg and ffprobe not found; the web player plays only MP4 and WebM files the browser reads itself"
                .to_string(),
        ),
    };
    Check { check: "ffmpeg".into(), status, detail }
}
