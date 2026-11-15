//! Writes `web/src/wire.ts`, the web app's types, from the Rust types that make the JSON.

use std::path::Path;

use torrential_core::PieceMap;
use torrential_core::watching::{FINISHED_SHARE, FINISHED_WITHIN, SAVE_EVERY, WORTH_SAVING};
use ts_rs::{Config, TS};

use super::cast::Start;
use super::devices::{Me, Offer, Shown};
use super::online::{Listed, Named, Online};
use super::play::Playing;
use super::titles::{Forget, Found, Measured, Play, PlayingLine, Position, Saved};
use super::{FfmpegLine, Pieces, SearchLine, VersionReply};
use crate::daemon::devices::OFFER;
use crate::daemon::downloads::{PIECE_RUNS, Served};
use crate::daemon::media::thumbnail;
use crate::daemon::media::tracks::{Audio, Chapter, Media, Subtitle, Video};
use crate::ffmpeg::fetch::Readiness;

const HEADER: &str = "// The engine's JSON, as the Rust types that write it describe it. `cargo test` writes this file\n\
// from crates/core and crates/torrential/src/daemon/web, and fails when it was out of date.\n\n";

/// The web app's wire types: core's, then the engine's own, each declared once.
fn declarations(cfg: &Config) -> Vec<String> {
    let mut all = torrential_core::ts::declarations(cfg);
    all.extend([
        Served::decl(cfg),
        Playing::decl(cfg),
        Readiness::decl(cfg),
        Video::decl(cfg),
        Audio::decl(cfg),
        Subtitle::decl(cfg),
        Chapter::decl(cfg),
        Media::decl(cfg),
        Named::decl(cfg),
        Listed::decl(cfg),
        Online::decl(cfg),
        Pieces::decl(cfg),
        VersionReply::decl(cfg),
        SearchLine::decl(cfg),
        FfmpegLine::decl(cfg),
        Found::decl(cfg),
        Measured::decl(cfg),
        Play::decl(cfg),
        PlayingLine::decl(cfg),
        Position::decl(cfg),
        Saved::decl(cfg),
        Forget::decl(cfg),
        Offer::decl(cfg),
        Shown::decl(cfg),
        Me::decl(cfg),
        Start::decl(cfg),
    ]);
    all
}

/// Numbers the web app and the engine must agree on: each name, what it is, and its value.
fn constants() -> [(&'static str, &'static str, String); 12] {
    [
        ("SHORT_ID", "Hex digits every screen shows of an ID.", torrential_core::id::SHORT_ID.to_string()),
        ("SLICES", "Equal runs of a torrent's pieces in `Progress.piece_map`.", PieceMap::SLICES.to_string()),
        ("PIECE_RUNS", "Equal runs of a file's bytes in `GET /api/pieces`.", PIECE_RUNS.to_string()),
        ("SEARCH_LIMIT", "Results `POST /api/search` answers when it names no limit.", super::SEARCH_LIMIT.to_string()),
        ("PAIRING_MINUTES", "How long a pairing code works.", (OFFER.as_secs() / 60).to_string()),
        ("THUMBNAIL_EVERY", "Seconds of video one thumbnail stands for.", thumbnail::EVERY.to_string()),
        (
            "THUMBNAIL_MARGIN_BYTES",
            "Bytes a thumbnail needs on disk each side of its frame.",
            thumbnail::MARGIN.to_string(),
        ),
        (
            "THUMBNAIL_MARGIN_SHARE",
            "The share of a large file a thumbnail needs on disk each side of its frame.",
            (1.0 / thumbnail::MARGIN_SHARE as f64).to_string(),
        ),
        ("FINISHED_SHARE", "The share of a video at its end that counts as credits.", FINISHED_SHARE.to_string()),
        ("FINISHED_WITHIN", "Seconds at the end of a video that count as credits.", FINISHED_WITHIN.to_string()),
        ("WORTH_SAVING", "Seconds at the start of a video too few to keep a place at.", WORTH_SAVING.to_string()),
        ("SAVE_EVERY_MS", "How often a player saves where it is.", SAVE_EVERY.as_millis().to_string()),
    ]
}

#[test]
fn the_web_apps_types_are_the_ones_the_engine_writes() {
    let cfg = Config::new().with_large_int("number");
    let mut text = String::from(HEADER);
    for declaration in declarations(&cfg) {
        let lines: Vec<&str> = declaration.lines().map(str::trim_end).collect();
        text.push_str(&format!("export {}\n\n", lines.join("\n")));
    }
    for (name, what, value) in constants() {
        text.push_str(&format!("/** {what} */\nexport const {name} = {value};\n\n"));
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/wire.ts");
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    if committed != text {
        std::fs::write(&path, &text).expect("web/src is writable");
        panic!("web/src/wire.ts was out of date, and this test rewrote it: commit the new one");
    }
}
