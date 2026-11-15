//! Writes `web/src/wire.ts`, the web app's types, from the Rust types that make the JSON.

use std::path::Path;

use ts_rs::{Config, TS};

use super::cast::Start;
use super::devices::{Me, Offer, Shown};
use super::media::{Audio, Chapter, Media, Subtitle, Video};
use super::online::{Listed, Named, Online};
use super::play::Playing;
use super::titles::{Found, Measured, Play, PlayingLine, Saved};
use super::{FfmpegLine, Pieces, SearchLine, VersionReply};
use crate::daemon::downloads::Served;
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
        Saved::decl(cfg),
        Offer::decl(cfg),
        Shown::decl(cfg),
        Me::decl(cfg),
        Start::decl(cfg),
    ]);
    all
}

#[test]
fn the_web_apps_types_are_the_ones_the_engine_writes() {
    let cfg = Config::new().with_large_int("number");
    let mut text = String::from(HEADER);
    for declaration in declarations(&cfg) {
        let lines: Vec<&str> = declaration.lines().map(str::trim_end).collect();
        text.push_str(&format!("export {}\n\n", lines.join("\n")));
    }
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/wire.ts");
    let committed = std::fs::read_to_string(&path).unwrap_or_default();
    if committed != text {
        std::fs::write(&path, &text).expect("web/src is writable");
        panic!("web/src/wire.ts was out of date, and this test rewrote it: commit the new one");
    }
}
