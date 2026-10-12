//! Durable file primitives: bounded reads, fsync, and atomic replacement.
//!
//! Crash-safety contract (documented in `docs/file-format.md`):
//! a snapshot is written to a temporary file in the same directory,
//! flushed with `fsync`, then atomically moved over the destination.
//! On Unix the parent directory is fsynced afterwards so the rename itself
//! is durable; on Windows `MoveFileExW` is called with
//! `MOVEFILE_WRITE_THROUGH`, which returns only after the move is flushed.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::error::{GraphError, Result};

/// Read a whole file, refusing files larger than `max_bytes`.
pub(crate) fn read_limited(path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| GraphError::io(format!("opening {}", path.display()), e))?;
    let len = file.metadata().map_err(|e| GraphError::io(format!("reading metadata of {}", path.display()), e))?.len();
    if len > max_bytes {
        return Err(GraphError::LimitExceeded(format!(
            "{} is {len} bytes; the configured limit is {max_bytes}",
            path.display()
        )));
    }
    let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    // `take` guards against the file growing between metadata and read.
    file.take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|e| GraphError::io(format!("reading {}", path.display()), e))?;
    if buf.len() as u64 > max_bytes {
        return Err(GraphError::LimitExceeded(format!("{} exceeds {max_bytes} bytes", path.display())));
    }
    Ok(buf)
}

/// Path with a suffix appended to the file name (`a.graph` -> `a.graph.journal`).
pub(crate) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    path.with_file_name(name)
}

pub(crate) fn create_truncate(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create(true).truncate(true).open(path)
}

/// Atomically replace `dst` with `src` and make the replacement durable.
pub(crate) fn atomic_replace(src: &Path, dst: &Path, sync: bool) -> io::Result<()> {
    platform::replace(src, dst, sync)?;
    if sync {
        sync_parent_dir(dst)?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> io::Result<()> {
    // Directory handles cannot be fsynced on Windows; MOVEFILE_WRITE_THROUGH
    // provides the equivalent guarantee for the rename.
    Ok(())
}

pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod platform {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::time::Duration;

    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW};

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    pub(super) fn replace(src: &Path, dst: &Path, sync: bool) -> io::Result<()> {
        let (s, d) = (wide(src), wide(dst));
        let mut flags = MOVEFILE_REPLACE_EXISTING;
        if sync {
            flags |= MOVEFILE_WRITE_THROUGH;
        }
        // Virus scanners and indexers briefly hold files open on Windows;
        // retry sharing violations with a short back-off.
        let mut attempt = 0u32;
        loop {
            // SAFETY: both buffers are valid, NUL-terminated UTF-16 strings
            // that outlive the call.
            let ok = unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), flags) };
            if ok != 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            // 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION
            let transient = matches!(err.raw_os_error(), Some(5 | 32));
            if !transient || attempt >= 20 {
                return Err(err);
            }
            attempt += 1;
            std::thread::sleep(Duration::from_millis(10 * u64::from(attempt)));
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::io;
    use std::path::Path;

    pub(super) fn replace(src: &Path, dst: &Path, _sync: bool) -> io::Result<()> {
        std::fs::rename(src, dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn replace_overwrites_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("a.graph");
        let tmp = sibling(&dst, ".tmp");
        fs::write(&dst, b"old").expect("write");
        let mut f = create_truncate(&tmp).expect("tmp");
        f.write_all(b"new").expect("write");
        f.sync_all().expect("sync");
        drop(f);
        atomic_replace(&tmp, &dst, true).expect("replace");
        assert_eq!(fs::read(&dst).expect("read"), b"new");
        assert!(!tmp.exists());
    }

    #[test]
    fn read_limited_enforces_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("x");
        fs::write(&p, vec![b'a'; 100]).expect("write");
        assert!(read_limited(&p, 100).is_ok());
        assert!(matches!(read_limited(&p, 99), Err(GraphError::LimitExceeded(_))));
    }
}
