//! `torrential web`: opens the web app the engine serves.

use serde::Serialize;
use torrential_core::{Error, ErrorCode};

use super::connect::Conn;
use super::emit;
use crate::cli::WebArgs;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};
use torrential_daemon::env::Env;

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
        torrential_daemon::player::open_url(&url, env)?;
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
    Ok(emit(mode, &doc, &Typed { kind: DocKind::Web, body: Web { url, opened } }))
}
