//! The engine, and what the engine and the CLI both use: the environment and file layout, the
//! config, the login service, updates, the wire framing, and the playback rules. The `torrential`
//! crate is the CLI on top of it, and nothing here reads the CLI.

pub mod config;
pub mod daemon;
pub mod disk;
pub mod env;
pub mod exit;
pub mod ffmpeg;
pub mod folder;
pub mod fsutil;
pub mod log;
pub mod machine;
pub mod paths;
pub mod playback;
pub mod player;
pub mod process;
pub mod qr;
pub mod selection;
pub mod service;
pub mod update;
pub mod wire;
