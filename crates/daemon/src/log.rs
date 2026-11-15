//! The engine's log: a line on stderr for each event, which launchd and systemd keep in a file.

/// Writes `message` to the log after the time.
pub fn log(message: &str) {
    eprintln!("{} {message}", torrential_core::units::timestamp(torrential_core::time::now()));
}

/// The one policy for a write the engine can go on without: log what failed, and go on from what
/// is in memory. Says whether it worked. `what` reads after "cannot", such as "save the progress".
pub fn persist(what: &str, result: Result<(), impl std::fmt::Display>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            log(&format!("cannot {what}: {error}"));
            false
        }
    }
}
