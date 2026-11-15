//! The names of the variables a test sets to change what torrential does. The end-to-end tests
//! include this file, so a name is spelled once.

/// `fake` runs the made-up engine, swarm, search and catalogs instead of libtorrent and the network.
pub const ENGINE: &str = "TORRENTIAL_TEST_ENGINE";
/// `loopback` keeps libtorrent off the public network, with the peers in `PEERS`.
pub const NETWORK: &str = "TORRENTIAL_TEST_NETWORK";
/// Comma-separated addresses of the peers on the loopback network.
pub const PEERS: &str = "TORRENTIAL_TEST_PEERS";
/// How fast the fake swarm sends, as a size per second.
pub const FAKE_RATE: &str = "TORRENTIAL_TEST_FAKE_RATE";
/// Comma-separated IDs of torrents no fake peer sends a piece of.
pub const DEAD: &str = "TORRENTIAL_TEST_DEAD";
/// `live` gives the fake engine the real subtitle sources.
pub const SUBTITLES: &str = "TORRENTIAL_TEST_SUBTITLES";
/// `live` gives the fake engine the real title catalog.
pub const CATALOG: &str = "TORRENTIAL_TEST_CATALOG";
/// The address TV searches go to instead of the multicast group.
pub const SSDP: &str = "TORRENTIAL_TEST_SSDP";
/// `roomy` reports plenty of free space on every disk.
pub const DISK: &str = "TORRENTIAL_TEST_DISK";
/// How long a play leaving the plays folder waits before it goes.
pub const DELETION_PAUSE: &str = "TORRENTIAL_TEST_DELETION_PAUSE";
/// How long a file waits between its last check and its move to the Trash.
pub const TRASH_PAUSE: &str = "TORRENTIAL_TEST_TRASH_PAUSE";
/// How long a play no one streams keeps downloading.
pub const PLAYS_IDLE: &str = "TORRENTIAL_TEST_PLAYS_IDLE";
/// The folder that stands in for the Trash.
pub const TRASH: &str = "TORRENTIAL_TEST_TRASH";
/// How often the engine saves each download's resume data.
pub const SAVE_RESUME_EVERY: &str = "TORRENTIAL_TEST_SAVE_RESUME_EVERY";
/// How long a download without progress waits before it counts as stalled.
pub const STALL_AFTER: &str = "TORRENTIAL_TEST_STALL_AFTER";
/// How long a title play waits for a file list and for its first bytes.
pub const TITLE_WAIT: &str = "TORRENTIAL_TEST_TITLE_WAIT";
/// The port the engine serves the web app and players on.
pub const HTTP_PORT: &str = "TORRENTIAL_TEST_HTTP_PORT";
/// `name=url` pairs that replace the search sources' addresses.
pub const ENDPOINTS: &str = "TORRENTIAL_TEST_ENDPOINTS";
/// `name=url` pairs that replace the subtitle sources' addresses.
pub const SUBTITLE_ENDPOINTS: &str = "TORRENTIAL_TEST_SUBTITLE_ENDPOINTS";
/// The address of the title catalog.
pub const CATALOG_ENDPOINT: &str = "TORRENTIAL_TEST_CATALOG_ENDPOINT";
/// The version torrential reports instead of its own.
pub const VERSION: &str = "TORRENTIAL_TEST_VERSION";
/// The machine, as JSON, instead of the one torrential detects.
pub const MACHINE: &str = "TORRENTIAL_TEST_MACHINE";
/// The address of the releases page that updates read.
pub const RELEASES: &str = "TORRENTIAL_TEST_RELEASES";
/// A folder of stand-ins for launchctl and systemctl.
pub const SERVICE_BIN: &str = "TORRENTIAL_TEST_SERVICE_BIN";
/// The program that opens a URL in place of the browser.
pub const OPENER: &str = "TORRENTIAL_TEST_OPENER";
