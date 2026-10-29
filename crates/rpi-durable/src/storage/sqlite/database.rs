//! Re-export of the chord SQLite driver facade.
//!
//! Upstream moved the portable driver interfaces and their Node adapter into
//! `@earendil-works/chord/sqlite`; the durable storage entry points re-export
//! the same names. This module keeps `rpi_durable::storage::sqlite::database::*`
//! working.

pub use crate::chord::sqlite::database::*;
