//! Append-only transaction journal (write-ahead log).
//!
//! Layout: one header line followed by one line per committed transaction.
//!
//! ```text
//! {"journal":"graphydra","version":1,"graph_id":"..."}
//! 1a2b3c4d {"gen":7,"ops":[...]}
//! ```
//!
//! Each record is prefixed with the CRC-32 of its JSON payload. A commit is
//! durable once its line has been written and fsynced. On open, records whose
//! generation is not newer than the snapshot are skipped (they were already
//! checkpointed); an incomplete or corrupt *final* line is a torn write and is
//! discarded; corruption anywhere else is reported as an error.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{GraphError, Result};
use crate::ops::Op;
use crate::storage::fsutil;

const JOURNAL_MAGIC: &str = "graphydra";
const JOURNAL_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    journal: String,
    version: u32,
    graph_id: String,
}

#[derive(Serialize)]
struct RecordOut<'a> {
    #[serde(rename = "gen")]
    generation: u64,
    ops: &'a [Op],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordIn {
    #[serde(rename = "gen")]
    generation: u64,
    ops: Vec<Op>,
}

/// A committed transaction read back from the journal.
#[derive(Debug)]
pub(crate) struct JournalRecord {
    pub(crate) generation: u64,
    pub(crate) ops: Vec<Op>,
}

/// Result of scanning a journal file.
pub(crate) struct JournalScan {
    pub(crate) records: Vec<JournalRecord>,
    /// Byte offset just past the last valid record.
    pub(crate) valid_len: u64,
    /// True if bytes after `valid_len` were discarded (torn write).
    pub(crate) torn_tail: bool,
    /// True if the header is missing or incomplete (file never finished
    /// initialising); the journal holds no committed data in that case.
    pub(crate) needs_init: bool,
}

fn header_line(graph_id: &str) -> Result<String> {
    let h = Header { journal: JOURNAL_MAGIC.into(), version: JOURNAL_VERSION, graph_id: graph_id.into() };
    let json = serde_json::to_string(&h).map_err(|e| GraphError::Corrupt(e.to_string()))?;
    Ok(format!("{json}\n"))
}

/// Scan journal bytes. Pure function so it can be tested exhaustively.
pub(crate) fn scan(bytes: &[u8], graph_id: &str) -> Result<JournalScan> {
    let empty = |needs_init| JournalScan { records: Vec::new(), valid_len: 0, torn_tail: false, needs_init };
    let Some(header_end) = bytes.iter().position(|b| *b == b'\n') else {
        // Empty, or crashed while writing the header.
        return Ok(empty(true));
    };
    let header: Header = serde_json::from_slice(&bytes[..header_end])
        .map_err(|e| GraphError::Corrupt(format!("journal header is invalid: {e}")))?;
    if header.journal != JOURNAL_MAGIC || header.version != JOURNAL_VERSION {
        return Err(GraphError::Corrupt(format!(
            "unsupported journal {:?} version {}",
            header.journal, header.version
        )));
    }
    if header.graph_id != graph_id {
        return Err(GraphError::Corrupt(format!("journal belongs to graph {:?}, not {graph_id:?}", header.graph_id)));
    }

    let mut records = Vec::new();
    let mut pos = header_end + 1;
    let mut valid_len = pos as u64;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        let Some(nl) = rest.iter().position(|b| *b == b'\n') else {
            // Unterminated final line: torn write.
            return Ok(JournalScan { records, valid_len, torn_tail: true, needs_init: false });
        };
        let line = &rest[..nl];
        let is_last = pos + nl + 1 >= bytes.len();
        match parse_record(line) {
            Ok(rec) => records.push(rec),
            Err(_) if is_last => {
                return Ok(JournalScan { records, valid_len, torn_tail: true, needs_init: false });
            }
            Err(e) => {
                return Err(GraphError::Corrupt(format!("journal record at byte {pos} is damaged: {e}")));
            }
        }
        pos += nl + 1;
        valid_len = pos as u64;
    }
    Ok(JournalScan { records, valid_len, torn_tail: false, needs_init: false })
}

fn parse_record(line: &[u8]) -> std::result::Result<JournalRecord, String> {
    if line.len() < 10 || line[8] != b' ' {
        return Err("malformed record framing".into());
    }
    let crc_text = std::str::from_utf8(&line[..8]).map_err(|_| "bad checksum field")?;
    let expected = u32::from_str_radix(crc_text, 16).map_err(|_| "bad checksum field")?;
    let payload = &line[9..];
    if crc32fast::hash(payload) != expected {
        return Err("checksum mismatch".into());
    }
    let rec: RecordIn = serde_json::from_slice(payload).map_err(|e| e.to_string())?;
    Ok(JournalRecord { generation: rec.generation, ops: rec.ops })
}

/// Encode one record line (including trailing newline).
pub(crate) fn encode_record(generation: u64, ops: &[Op]) -> Result<Vec<u8>> {
    let payload = serde_json::to_vec(&RecordOut { generation, ops })
        .map_err(|e| GraphError::Validation(format!("cannot encode transaction: {e}")))?;
    let mut line = format!("{:08x} ", crc32fast::hash(&payload)).into_bytes();
    line.extend_from_slice(&payload);
    line.push(b'\n');
    Ok(line)
}

/// Writable journal handle owned by the single writer.
pub(crate) struct Journal {
    file: File,
    path: PathBuf,
    len: u64,
    header_len: u64,
    sync: bool,
    graph_id: String,
}

impl Journal {
    /// Open (creating or repairing as needed) and scan the journal.
    pub(crate) fn open(path: &Path, graph_id: &str, sync: bool, max_bytes: u64) -> Result<(Journal, JournalScan)> {
        let ctx = |what: &str| format!("{what} journal {}", path.display());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| GraphError::io(ctx("opening"), e))?;
        let bytes = fsutil::read_limited(path, max_bytes)?;
        let scan = scan(&bytes, graph_id)?;
        let header = header_line(graph_id)?;
        let mut journal = Journal {
            file,
            path: path.to_owned(),
            len: 0,
            header_len: header.len() as u64,
            sync,
            graph_id: graph_id.to_owned(),
        };
        if scan.needs_init {
            journal.reset()?;
        } else {
            journal.len = scan.valid_len;
            if scan.torn_tail {
                // Discard the torn record so future appends start cleanly.
                journal.file.set_len(scan.valid_len).map_err(|e| GraphError::io(ctx("truncating"), e))?;
                journal.sync_file()?;
            }
        }
        Ok((journal, scan))
    }

    /// Read-only scan without creating or repairing anything.
    pub(crate) fn read_only_scan(path: &Path, graph_id: &str, max_bytes: u64) -> Result<JournalScan> {
        if !path.exists() {
            return Ok(JournalScan { records: Vec::new(), valid_len: 0, torn_tail: false, needs_init: true });
        }
        let bytes = fsutil::read_limited(path, max_bytes)?;
        scan(&bytes, graph_id)
    }

    /// Bytes of committed records not yet checkpointed.
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.len.saturating_sub(self.header_len)
    }

    fn sync_file(&self) -> Result<()> {
        if self.sync {
            self.file.sync_data().map_err(|e| GraphError::io(format!("syncing {}", self.path.display()), e))?;
        }
        Ok(())
    }

    /// Append one encoded record and make it durable. On failure the journal
    /// is rolled back to its previous length; if even that fails the error
    /// is `Poisoned`, because the on-disk state is unknown.
    pub(crate) fn append(&mut self, line: &[u8]) -> Result<()> {
        let start = self.len;
        let result = (|| -> std::io::Result<()> {
            self.file.seek(SeekFrom::Start(start))?;
            self.file.write_all(line)?;
            if self.sync {
                self.file.sync_data()?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.len = start + line.len() as u64;
                Ok(())
            }
            Err(e) => {
                let restored = self.file.set_len(start).and_then(|()| self.file.sync_data());
                match restored {
                    Ok(()) => Err(GraphError::io(format!("appending to {}", self.path.display()), e)),
                    Err(e2) => {
                        Err(GraphError::Poisoned(format!("journal write failed ({e}) and could not be undone ({e2})")))
                    }
                }
            }
        }
    }

    /// Truncate to just the header (after a checkpoint, or on first use).
    pub(crate) fn reset(&mut self) -> Result<()> {
        let header = header_line(&self.graph_id)?;
        let ctx = |e| GraphError::io(format!("resetting {}", self.path.display()), e);
        self.file.set_len(0).map_err(ctx)?;
        self.file.seek(SeekFrom::Start(0)).map_err(ctx)?;
        self.file.write_all(header.as_bytes()).map_err(ctx)?;
        if self.sync {
            self.file.sync_data().map_err(ctx)?;
        }
        self.header_len = header.len() as u64;
        self.len = self.header_len;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;

    fn journal_bytes(records: &[(u64, Vec<Op>)]) -> Vec<u8> {
        let mut bytes = header_line("g").expect("header").into_bytes();
        for (generation, ops) in records {
            bytes.extend(encode_record(*generation, ops).expect("encode"));
        }
        bytes
    }

    fn add(id: &str) -> Vec<Op> {
        vec![Op::AddNode { node: Node::new(id) }]
    }

    #[test]
    fn round_trips_records() {
        let bytes = journal_bytes(&[(1, add("a")), (2, add("b"))]);
        let s = scan(&bytes, "g").expect("scan");
        assert_eq!(s.records.len(), 2);
        assert_eq!(s.records[1].generation, 2);
        assert_eq!(s.valid_len, bytes.len() as u64);
        assert!(!s.torn_tail);
    }

    #[test]
    fn every_truncation_point_is_recoverable() {
        let bytes = journal_bytes(&[(1, add("a")), (2, add("b"))]);
        let first_end = header_line("g").expect("header").len() + encode_record(1, &add("a")).expect("encode").len();
        for cut in 0..bytes.len() {
            let s = scan(&bytes[..cut], "g").expect("truncated journal must scan");
            let expected = usize::from(cut >= first_end);
            assert_eq!(s.records.len(), expected, "cut at {cut}");
            assert!(s.valid_len as usize <= cut);
        }
    }

    #[test]
    fn detects_corruption_before_tail() {
        let mut bytes = journal_bytes(&[(1, add("a")), (2, add("b"))]);
        let header_len = header_line("g").expect("header").len();
        bytes[header_len + 12] ^= 0x01;
        assert!(matches!(scan(&bytes, "g"), Err(GraphError::Corrupt(_))));
        // Corrupting only the final record is treated as a torn write.
        let mut bytes = journal_bytes(&[(1, add("a")), (2, add("b"))]);
        let n = bytes.len();
        bytes[n - 5] ^= 0x01;
        let s = scan(&bytes, "g").expect("scan");
        assert_eq!(s.records.len(), 1);
        assert!(s.torn_tail);
    }

    #[test]
    fn rejects_foreign_journal() {
        let bytes = journal_bytes(&[]);
        assert!(matches!(scan(&bytes, "other"), Err(GraphError::Corrupt(_))));
    }
}
