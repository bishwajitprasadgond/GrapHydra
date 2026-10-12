//! Persistence: canonical snapshot file, journal, locking and durable I/O.

pub(crate) mod format;
pub(crate) mod fsutil;
pub(crate) mod journal;
pub(crate) mod lock;

pub use format::{FORMAT_NAME, FORMAT_VERSION};
