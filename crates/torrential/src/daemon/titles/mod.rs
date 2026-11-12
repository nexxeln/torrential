//! Films and series: finding one, playing it from the first source that works, and where a
//! person stopped.

pub mod catalog_port;

use std::sync::Arc;

use catalog_port::CatalogPort;

/// What the engine knows about films and series, beside its downloads.
pub struct Titles {
    pub catalog: Arc<dyn CatalogPort>,
}
