//! Configuration: resource limits, durability and execution defaults.

use std::time::Duration;

/// Hard limits that protect against malicious files and runaway queries.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum size of a `.graph` file or import file, in bytes.
    pub max_file_bytes: u64,
    /// Maximum nesting depth of a property value.
    pub max_nesting_depth: usize,
    /// Maximum size of one string value, in bytes.
    pub max_string_bytes: usize,
    /// Maximum size of a node or edge id, in bytes.
    pub max_id_bytes: usize,
    /// Maximum size of a label, edge type or property key, in bytes.
    pub max_name_bytes: usize,
    /// Maximum size of query text, in bytes.
    pub max_query_bytes: usize,
    /// Largest permitted upper bound of a variable-length pattern.
    pub max_traversal_depth: u32,
    /// Maximum number of rows a query may return.
    pub max_result_rows: usize,
    /// Maximum number of intermediate rows materialised by sorting,
    /// aggregation, DISTINCT or mutations.
    pub max_intermediate_rows: usize,
    /// Maximum number of nodes + edges a query may examine.
    pub max_visits: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_file_bytes: 8 * 1024 * 1024 * 1024,
            max_nesting_depth: 32,
            max_string_bytes: 16 * 1024 * 1024,
            max_id_bytes: 1024,
            max_name_bytes: 256,
            max_query_bytes: 1024 * 1024,
            max_traversal_depth: 64,
            max_result_rows: 10_000_000,
            max_intermediate_rows: 50_000_000,
            max_visits: 2_000_000_000,
        }
    }
}

/// How hard commits work to reach stable storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// `fsync` the journal on every commit and the snapshot on checkpoint.
    #[default]
    Full,
    /// Skip `fsync`. Committed data survives a process crash but may be lost
    /// on power failure or OS crash. Useful for bulk loads and tests.
    NoSync,
}

/// Options used when opening a graph.
#[derive(Debug, Clone)]
pub struct Config {
    pub limits: Limits,
    pub durability: Durability,
    /// Journal size that triggers an automatic checkpoint after a commit.
    pub checkpoint_journal_bytes: u64,
    /// Default timeout applied to queries that do not set one.
    pub default_query_timeout: Option<Duration>,
    /// Number of parsed queries kept in the statement cache.
    pub statement_cache_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            limits: Limits::default(),
            durability: Durability::Full,
            checkpoint_journal_bytes: 64 * 1024 * 1024,
            default_query_timeout: None,
            statement_cache_size: 256,
        }
    }
}
