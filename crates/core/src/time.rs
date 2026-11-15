//! The wall clock.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch, or 0 when the clock reads before it.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
