//! The engine's log: a line on stderr for each event, which launchd and systemd keep in a file.

/// Writes `message` to the log after the time.
pub fn log(message: &str) {
    eprintln!("{} {message}", torrential_core::units::timestamp(torrential_core::time::now()));
}
