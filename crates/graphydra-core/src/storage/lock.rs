//! Single-writer lock.
//!
//! A writable handle holds an exclusive OS lock (`flock` / `LockFileEx`) on
//! `<file>.lock` for its whole lifetime. A second writer, in this or any
//! other process, fails fast with [`GraphError::Locked`]. Readers take no
//! lock: they load a consistent snapshot plus committed journal records.
//! Locks are advisory and are not reliable on network filesystems.

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::error::{GraphError, Result};

#[derive(Debug)]
pub(crate) struct WriterLock {
    _file: File,
}

impl WriterLock {
    pub(crate) fn acquire(lock_path: &Path, graph_path: &Path) -> Result<WriterLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| GraphError::io(format!("opening lock file {}", lock_path.display()), e))?;
        match file.try_lock() {
            Ok(()) => Ok(WriterLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(GraphError::Locked { path: graph_path.to_owned() }),
            Err(std::fs::TryLockError::Error(e)) => Err(GraphError::io(format!("locking {}", lock_path.display()), e)),
        }
    }
}
