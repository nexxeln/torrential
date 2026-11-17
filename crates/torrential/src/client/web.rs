//! `torrential web`: opens the web app the engine serves.

use serde::Serialize;
use torrential_core::Error;

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

/// Opens the web app of the engine this torrential drives. A browser that is not paired with an
/// engine on another machine gets its pairing page first.
pub fn run(conn: &Conn, args: WebArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let url = conn.engine.address().to_string();
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
