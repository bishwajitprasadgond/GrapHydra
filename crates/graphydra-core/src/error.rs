//! Error types for every layer of the library.
//!
//! All fallible public functions return [`Result<T>`]. Errors carry enough
//! context to be actionable but never include property *values* by default,
//! so they are safe to log when graphs contain sensitive data.

use std::fmt;
use std::path::PathBuf;

/// Convenience alias used throughout the crate.
pub type Result<T, E = GraphError> = std::result::Result<T, E>;

/// Kind of graph entity referenced by an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Node,
    Edge,
}

impl fmt::Display for EntityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EntityKind::Node => "node",
            EntityKind::Edge => "edge",
        })
    }
}

/// Position-aware syntax error raised by the query parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub message: String,
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number (in characters).
    pub column: usize,
    /// The offending source line, for display.
    pub snippet: String,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at line {}, column {}", self.message, self.line, self.column)?;
        if !self.snippet.is_empty() {
            write!(f, "\n  {}\n  {}^", self.snippet, " ".repeat(self.column.saturating_sub(1)))?;
        }
        Ok(())
    }
}

/// The single error type of the library.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// Underlying I/O failure.
    #[error("I/O error while {context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// The `.graph` file (or an import file) is structurally invalid.
    #[error("invalid graph file: {0}")]
    Format(String),

    /// The file declares a format version this build cannot read.
    #[error("unsupported format version {found:?}; this build supports {supported}")]
    UnsupportedVersion { found: String, supported: String },

    /// Data violates a model rule (bad id, bad label, value limits, ...).
    #[error("validation failed: {0}")]
    Validation(String),

    #[error("query syntax error: {0}")]
    Syntax(SyntaxError),

    /// Query is syntactically valid but semantically wrong (unknown variable, ...).
    #[error("invalid query: {0}")]
    Semantic(String),

    /// Runtime type error during expression evaluation.
    #[error("type error: {0}")]
    Type(String),

    /// Missing or invalid query parameter.
    #[error("parameter error: {0}")]
    Parameter(String),

    #[error("{kind} {id:?} not found")]
    NotFound { kind: EntityKind, id: String },

    #[error("{kind} id {id:?} already exists")]
    DuplicateId { kind: EntityKind, id: String },

    /// Referential-integrity violation: an edge endpoint does not exist.
    #[error("edge {edge:?} references missing node {node:?}")]
    DanglingEdge { edge: String, node: String },

    /// Node deletion refused because it still has incident edges.
    #[error("node {id:?} still has {count} incident edge(s); use DETACH DELETE or detach=True")]
    NodeHasEdges { id: String, count: usize },

    #[error("graph is opened read-only")]
    ReadOnly,

    #[error("graph {path:?} is locked by another writer")]
    Locked { path: PathBuf },

    #[error("transaction error: {0}")]
    Transaction(String),

    #[error("query exceeded its time limit")]
    Timeout,

    #[error("query was cancelled")]
    Cancelled,

    /// A configured resource limit was hit (rows, visits, depth, sizes, ...).
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    /// The canonical file changed on disk behind this process's back.
    #[error("{0:?} was modified outside GrapHydra; refusing to overwrite it (reopen the graph)")]
    ExternalModification(PathBuf),

    /// The journal or a file is damaged beyond automatic recovery.
    #[error("corrupt data: {0}")]
    Corrupt(String),

    /// A previous write failed in a way that leaves on-disk state uncertain.
    #[error("graph handle is unusable after a failed write; reopen it: {0}")]
    Poisoned(String),

    #[error("index error: {0}")]
    Index(String),

    #[error("graph is closed")]
    Closed,
}

impl GraphError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        GraphError::Io { context: context.into(), source }
    }

    pub(crate) fn semantic(msg: impl Into<String>) -> Self {
        GraphError::Semantic(msg.into())
    }

    pub(crate) fn type_err(msg: impl Into<String>) -> Self {
        GraphError::Type(msg.into())
    }

    pub(crate) fn validation(msg: impl Into<String>) -> Self {
        GraphError::Validation(msg.into())
    }
}
