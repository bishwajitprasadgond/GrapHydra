//! Offline validation of `.graph` files and their journals.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::Limits;
use crate::storage::format;
use crate::storage::fsutil::{read_limited, sibling};
use crate::storage::journal::Journal;

/// Result of validating a graph file. Validation never modifies anything.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ValidationReport {
    pub path: PathBuf,
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub graph_id: Option<String>,
    pub generation: u64,
    pub nodes: usize,
    pub edges: usize,
    /// Committed journal records not yet checkpointed into the snapshot.
    pub pending_journal_records: usize,
    pub sha256: Option<String>,
}

/// Checks file structure, identifiers, references, schema rules, the
/// journal, and the consistency of all derived indexes.
#[derive(Debug, Clone, Default)]
pub struct GraphValidator {
    pub limits: Limits,
}

impl GraphValidator {
    pub fn new(limits: Limits) -> Self {
        GraphValidator { limits }
    }

    pub fn validate_file(&self, path: impl AsRef<Path>) -> ValidationReport {
        let path = path.as_ref();
        let mut report = ValidationReport { path: path.to_path_buf(), ..ValidationReport::default() };
        let bytes = match read_limited(path, self.limits.max_file_bytes) {
            Ok(b) => b,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        report.sha256 = Some(format::hex(&format::sha256(&bytes)));
        let snap = match format::parse(&bytes, &self.limits) {
            Ok(s) => s,
            Err(e) => {
                report.errors.push(e.to_string());
                return report;
            }
        };
        drop(bytes);
        report.graph_id = Some(snap.graph_id.clone());
        let mut store = snap.store;
        let mut generation = snap.generation;

        let journal_path = sibling(path, ".journal");
        match Journal::read_only_scan(&journal_path, &snap.graph_id, self.limits.max_file_bytes) {
            Ok(scan) => {
                if scan.torn_tail {
                    report.warnings.push(
                        "journal ends with an incomplete record (interrupted write); it will be discarded on next open"
                            .into(),
                    );
                }
                for rec in scan.records {
                    if rec.generation <= generation {
                        continue;
                    }
                    if rec.generation != generation + 1 {
                        report.errors.push(format!("journal skips from generation {generation} to {}", rec.generation));
                        break;
                    }
                    if let Some(err) = rec.ops.iter().find_map(|op| store.apply(op).err()) {
                        report.errors.push(format!("journal record {} cannot be applied: {err}", rec.generation));
                        break;
                    }
                    generation = rec.generation;
                    report.pending_journal_records += 1;
                }
                if report.pending_journal_records > 0 {
                    report.warnings.push(format!(
                        "{} committed transaction(s) are only in the journal; open the graph for writing and checkpoint to fold them in",
                        report.pending_journal_records
                    ));
                }
            }
            Err(e) => report.errors.push(e.to_string()),
        }
        for problem in store.verify() {
            report.errors.push(format!("internal consistency: {problem}"));
        }
        report.generation = generation;
        report.nodes = store.node_count();
        report.edges = store.edge_count();
        report.valid = report.errors.is_empty();
        report
    }
}

/// Validate a file with default limits.
pub fn validate_file(path: impl AsRef<Path>) -> ValidationReport {
    GraphValidator::default().validate_file(path)
}
