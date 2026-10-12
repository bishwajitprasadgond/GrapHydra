//! The public [`Graph`] handle.
//!
//! # Persistence and transactions
//!
//! * `<file>.graph`: canonical snapshot (portable JSON).
//! * `<file>.graph.journal`: committed transactions not yet checkpointed.
//! * `<file>.graph.lock`: held by the single writer.
//!
//! Every mutation runs inside a transaction (an implicit one if none is
//! open). Commit appends one checksummed record to the journal and fsyncs
//! it; the transaction is durable from that moment. A checkpoint rewrites
//! the snapshot atomically and truncates the journal. Opening a graph
//! replays committed journal records and discards a torn final record.
//!
//! # Concurrency
//!
//! One writer per file (enforced with an OS lock) and any number of
//! read-only handles. A read-only handle sees the state committed when it
//! was opened; call [`Graph::refresh`] to load newer commits.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;

use crate::config::{Config, Durability};
use crate::engine::expr::walk_path;
use crate::engine::mutate::Mutator;
use crate::engine::result::{Budget, QueryOptions, QueryPlan, QueryResult, QueryStats, ResultValue};
use crate::engine::run_query;
use crate::error::{EntityKind, GraphError, Result};
use crate::language::ast::{
    ExplainMode, Expr, NodePattern, Pattern, Query, ReturnClause, ReturnItem, Statement, StatementKind,
};
use crate::language::parser::{parse_script, parse_statement};
use crate::model::{Edge, IndexDef, IndexKind, IndexTarget, Node, Path as GraphPath, validate_id};
use crate::ops::Op;
use crate::storage::format::{self, HashingWriter, sha256};
use crate::storage::fsutil::{self, atomic_replace, create_truncate, read_limited, remove_if_exists, sibling};
use crate::storage::journal::{Journal, JournalScan, encode_record};
use crate::storage::lock::WriterLock;
use crate::store::{Direction, Slot, Store, Undo};
use crate::value::{Properties, Value};

/// How to open a graph file.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Create the file if it does not exist.
    pub create: bool,
    /// Open without the writer lock; all mutations fail with `ReadOnly`.
    pub read_only: bool,
    /// Graph id for newly created files (defaults to the file stem).
    pub graph_id: Option<String>,
    pub config: Config,
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn create(mut self, yes: bool) -> Self {
        self.create = yes;
        self
    }

    #[must_use]
    pub fn read_only(mut self, yes: bool) -> Self {
        self.read_only = yes;
        self
    }

    #[must_use]
    pub fn graph_id(mut self, id: impl Into<String>) -> Self {
        self.graph_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }
}

/// Summary statistics of a graph.
#[derive(Debug, Clone, Serialize)]
pub struct GraphStats {
    pub graph_id: String,
    pub generation: u64,
    pub nodes: usize,
    pub edges: usize,
    pub labels: Vec<(String, usize)>,
    pub edge_types: Vec<(String, usize)>,
    pub indexes: Vec<IndexInfo>,
    pub journal_bytes: u64,
    /// Committed transactions replayed from the journal when opened.
    pub recovered_transactions: usize,
    /// True if a torn journal tail was discarded on open.
    pub recovered_torn_write: bool,
    pub fragmentation: f64,
    pub read_only: bool,
    pub in_transaction: bool,
    /// Last automatic checkpoint failure, if any (data is safe in the journal).
    pub last_checkpoint_error: Option<String>,
}

/// Description of a secondary index.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IndexInfo {
    #[serde(flatten)]
    pub def: IndexDef,
    /// Indexed records.
    pub entries: usize,
    /// Distinct keys (terms for text indexes).
    pub distinct_keys: usize,
}

struct Persistence {
    path: PathBuf,
    tmp_path: PathBuf,
    /// `None` for read-only handles.
    journal: Option<Journal>,
    _lock: Option<WriterLock>,
    snapshot_hash: [u8; 32],
}

#[derive(Default)]
struct TxLog {
    ops: Vec<Op>,
    undo: Vec<Undo>,
}

struct TxMutator<'g> {
    store: &'g mut Store,
    tx: &'g mut TxLog,
}

impl Mutator for TxMutator<'_> {
    fn store(&self) -> &Store {
        self.store
    }

    fn apply(&mut self, op: Op) -> Result<()> {
        let undo = self.store.apply(&op)?;
        self.tx.undo.push(undo);
        self.tx.ops.push(op);
        Ok(())
    }
}

struct ReadOnlyMutator<'g> {
    store: &'g Store,
}

impl Mutator for ReadOnlyMutator<'_> {
    fn store(&self) -> &Store {
        self.store
    }

    fn apply(&mut self, _op: Op) -> Result<()> {
        Err(GraphError::ReadOnly)
    }
}

#[derive(Default)]
struct StatementCache {
    map: HashMap<String, Arc<Statement>>,
    order: VecDeque<String>,
}

impl StatementCache {
    fn get_or_parse(&mut self, text: &str, capacity: usize) -> Result<(Arc<Statement>, bool)> {
        if let Some(s) = self.map.get(text) {
            return Ok((s.clone(), true));
        }
        let stmt = Arc::new(parse_statement(text)?);
        if capacity > 0 {
            if self.map.len() >= capacity
                && let Some(old) = self.order.pop_front()
            {
                self.map.remove(&old);
            }
            self.map.insert(text.to_owned(), stmt.clone());
            self.order.push_back(text.to_owned());
        }
        Ok((stmt, false))
    }
}

/// An embedded graph database: in memory or backed by a `.graph` file.
pub struct Graph {
    store: Store,
    graph_id: String,
    generation: u64,
    config: Config,
    read_only: bool,
    persistence: Option<Persistence>,
    tx: Option<TxLog>,
    poisoned: Option<String>,
    cache: StatementCache,
    recovered_transactions: usize,
    recovered_torn_write: bool,
    last_checkpoint_error: Option<String>,
}

impl std::fmt::Debug for Graph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Graph")
            .field("graph_id", &self.graph_id)
            .field("path", &self.path())
            .field("nodes", &self.store.node_count())
            .field("edges", &self.store.edge_count())
            .field("read_only", &self.read_only)
            .finish()
    }
}

fn default_graph_id(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("graph");
    if stem.is_empty() || stem.chars().any(char::is_control) { "graph".to_owned() } else { stem.to_owned() }
}

impl Graph {
    // ------------------------------------------------------------------
    // Construction
    // ------------------------------------------------------------------

    /// A new, empty, non-persistent graph.
    pub fn in_memory() -> Graph {
        Self::in_memory_with(Config::default())
    }

    pub fn in_memory_with(config: Config) -> Graph {
        Graph {
            store: Store::new(config.limits.clone()),
            graph_id: "memory".into(),
            generation: 0,
            config,
            read_only: false,
            persistence: None,
            tx: None,
            poisoned: None,
            cache: StatementCache::default(),
            recovered_transactions: 0,
            recovered_torn_write: false,
            last_checkpoint_error: None,
        }
    }

    /// Open an existing `.graph` file for writing.
    pub fn open(path: impl AsRef<Path>) -> Result<Graph> {
        Self::open_with(path, OpenOptions::new())
    }

    /// Open a file read-only (no lock is taken).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Graph> {
        Self::open_with(path, OpenOptions::new().read_only(true))
    }

    /// Create a new file (fails if it already exists).
    pub fn create(path: impl AsRef<Path>) -> Result<Graph> {
        let path = path.as_ref();
        if path.exists() {
            return Err(GraphError::io(
                format!("creating {}", path.display()),
                std::io::Error::new(std::io::ErrorKind::AlreadyExists, "file already exists"),
            ));
        }
        Self::open_with(path, OpenOptions::new().create(true))
    }

    pub fn open_with(path: impl AsRef<Path>, opts: OpenOptions) -> Result<Graph> {
        let path = path.as_ref().to_path_buf();
        let config = opts.config;
        let limits = config.limits.clone();
        let sync = config.durability == Durability::Full;
        let lock_path = sibling(&path, ".lock");
        let journal_path = sibling(&path, ".journal");
        let tmp_path = sibling(&path, ".tmp");

        let exists = path.exists();
        if !exists && (!opts.create || opts.read_only) {
            return Err(GraphError::io(
                format!("opening {}", path.display()),
                std::io::Error::new(std::io::ErrorKind::NotFound, "graph file does not exist"),
            ));
        }
        let lock = if opts.read_only { None } else { Some(WriterLock::acquire(&lock_path, &path)?) };

        if !exists {
            let graph_id = opts.graph_id.clone().unwrap_or_else(|| default_graph_id(&path));
            validate_id("graph", &graph_id, &limits)?;
            // Any journal next to a brand-new file belongs to something else.
            remove_if_exists(&journal_path).map_err(|e| GraphError::io("removing stale journal", e))?;
            write_snapshot(&path, &tmp_path, &graph_id, 0, &Store::new(limits.clone()), sync)?;
        } else if !opts.read_only {
            remove_if_exists(&tmp_path).map_err(|e| GraphError::io("removing stale temporary file", e))?;
        }

        let bytes = read_limited(&path, limits.max_file_bytes)?;
        let snapshot_hash = sha256(&bytes);
        let snap = format::parse(&bytes, &limits)?;
        drop(bytes);

        let (journal, scan) = if opts.read_only {
            (None, Journal::read_only_scan(&journal_path, &snap.graph_id, limits.max_file_bytes)?)
        } else {
            let (j, scan) = Journal::open(&journal_path, &snap.graph_id, sync, limits.max_file_bytes)?;
            (Some(j), scan)
        };

        let mut graph = Graph {
            store: snap.store,
            graph_id: snap.graph_id,
            generation: snap.generation,
            config,
            read_only: opts.read_only,
            persistence: Some(Persistence { path, tmp_path, journal, _lock: lock, snapshot_hash }),
            tx: None,
            poisoned: None,
            cache: StatementCache::default(),
            recovered_transactions: 0,
            recovered_torn_write: scan.torn_tail,
            last_checkpoint_error: None,
        };
        graph.replay(scan)?;
        Ok(graph)
    }

    fn replay(&mut self, scan: JournalScan) -> Result<()> {
        for rec in scan.records {
            if rec.generation <= self.generation {
                continue; // already contained in the snapshot
            }
            if rec.generation != self.generation + 1 {
                return Err(GraphError::Corrupt(format!(
                    "journal skips from generation {} to {}",
                    self.generation, rec.generation
                )));
            }
            for op in &rec.ops {
                self.store.apply(op).map_err(|e| {
                    GraphError::Corrupt(format!("journal replay failed at generation {}: {e}", rec.generation))
                })?;
            }
            self.generation = rec.generation;
            self.recovered_transactions += 1;
        }
        Ok(())
    }

    /// Reload a read-only handle from disk to see newer commits.
    pub fn refresh(&mut self) -> Result<()> {
        if !self.read_only {
            return Ok(());
        }
        let path = self.path().ok_or(GraphError::Closed)?.to_path_buf();
        let mut opts = OpenOptions::new().read_only(true);
        opts.config = self.config.clone();
        *self = Graph::open_with(path, opts)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Accessors
    // ------------------------------------------------------------------

    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }

    /// Number of committed transactions since the graph was created.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn path(&self) -> Option<&Path> {
        self.persistence.as_ref().map(|p| p.path.as_path())
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn in_transaction(&self) -> bool {
        self.tx.is_some()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn store_ref(&self) -> &Store {
        &self.store
    }

    pub fn node_count(&self) -> usize {
        self.store.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.store.edge_count()
    }

    pub fn metadata(&self) -> &Properties {
        &self.store.metadata
    }

    pub fn stats(&self) -> GraphStats {
        GraphStats {
            graph_id: self.graph_id.clone(),
            generation: self.generation,
            nodes: self.store.node_count(),
            edges: self.store.edge_count(),
            labels: self.store.label_stats(),
            edge_types: self.store.type_stats(),
            indexes: self.indexes(),
            journal_bytes: self.persistence.as_ref().and_then(|p| p.journal.as_ref()).map_or(0, Journal::pending_bytes),
            recovered_transactions: self.recovered_transactions,
            recovered_torn_write: self.recovered_torn_write,
            fragmentation: self.store.fragmentation(),
            read_only: self.read_only,
            in_transaction: self.tx.is_some(),
            last_checkpoint_error: self.last_checkpoint_error.clone(),
        }
    }

    // ------------------------------------------------------------------
    // Transactions
    // ------------------------------------------------------------------

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(GraphError::ReadOnly);
        }
        if let Some(p) = &self.poisoned {
            return Err(GraphError::Poisoned(p.clone()));
        }
        Ok(())
    }

    /// Start an explicit transaction.
    pub fn begin(&mut self) -> Result<()> {
        self.check_writable()?;
        if self.tx.is_some() {
            return Err(GraphError::Transaction("a transaction is already open (nesting is not supported)".into()));
        }
        self.tx = Some(TxLog::default());
        Ok(())
    }

    /// Commit the open transaction durably.
    pub fn commit(&mut self) -> Result<()> {
        let tx = self.tx.take().ok_or_else(|| GraphError::Transaction("no transaction is open".into()))?;
        self.commit_log(tx)
    }

    /// Discard all changes of the open transaction.
    pub fn rollback(&mut self) -> Result<()> {
        let tx = self.tx.take().ok_or_else(|| GraphError::Transaction("no transaction is open".into()))?;
        self.undo_all(tx.undo);
        Ok(())
    }

    /// Run `f` in a transaction: commit on `Ok`, roll back on `Err`.
    pub fn transaction<T>(&mut self, f: impl FnOnce(&mut Graph) -> Result<T>) -> Result<T> {
        self.begin()?;
        match f(self) {
            Ok(v) => {
                self.commit()?;
                Ok(v)
            }
            Err(e) => {
                if self.tx.is_some() {
                    self.rollback()?;
                }
                Err(e)
            }
        }
    }

    fn undo_all(&mut self, undo: Vec<Undo>) {
        for u in undo.into_iter().rev() {
            self.store.undo(u);
        }
    }

    fn commit_log(&mut self, tx: TxLog) -> Result<()> {
        if tx.ops.is_empty() {
            return Ok(());
        }
        let next = self.generation + 1;
        if let Some(journal) = self.persistence.as_mut().and_then(|p| p.journal.as_mut()) {
            let appended = encode_record(next, &tx.ops).and_then(|line| journal.append(&line));
            if let Err(e) = appended {
                if let GraphError::Poisoned(msg) = &e {
                    self.poisoned = Some(msg.clone());
                }
                self.undo_all(tx.undo);
                return Err(e);
            }
        }
        self.generation = next;
        self.maybe_auto_checkpoint();
        Ok(())
    }

    fn maybe_auto_checkpoint(&mut self) {
        let pending = self.persistence.as_ref().and_then(|p| p.journal.as_ref()).map_or(0, Journal::pending_bytes);
        if pending > self.config.checkpoint_journal_bytes {
            self.last_checkpoint_error = self.checkpoint().err().map(|e| e.to_string());
        }
    }

    /// Run `f` against the current (or an implicit) transaction. A failing
    /// statement is rolled back completely, even inside an explicit transaction.
    fn mutate<T>(&mut self, f: impl FnOnce(&mut dyn Mutator) -> Result<T>) -> Result<T> {
        self.check_writable()?;
        let implicit = self.tx.is_none();
        let mut tx = self.tx.take().unwrap_or_default();
        let mark = tx.undo.len();
        let result = f(&mut TxMutator { store: &mut self.store, tx: &mut tx });
        match result {
            Ok(v) => {
                if implicit {
                    self.commit_log(tx)?;
                } else {
                    self.tx = Some(tx);
                }
                Ok(v)
            }
            Err(e) => {
                let undo = tx.undo.split_off(mark);
                tx.ops.truncate(mark);
                self.undo_all(undo);
                if !implicit {
                    self.tx = Some(tx);
                }
                Err(e)
            }
        }
    }

    // ------------------------------------------------------------------
    // Persistence
    // ------------------------------------------------------------------

    /// Write a fresh snapshot and truncate the journal.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.check_writable()?;
        if self.tx.is_some() {
            return Err(GraphError::Transaction("cannot checkpoint while a transaction is open".into()));
        }
        let sync = self.config.durability == Durability::Full;
        let Some(p) = self.persistence.as_mut() else { return Ok(()) };
        let current = read_limited(&p.path, self.config.limits.max_file_bytes)?;
        if sha256(&current) != p.snapshot_hash {
            return Err(GraphError::ExternalModification(p.path.clone()));
        }
        drop(current);
        if self.store.fragmentation() > 0.25 {
            self.store = self.store.compacted();
        }
        let hash = write_snapshot(&p.path, &p.tmp_path, &self.graph_id, self.generation, &self.store, sync)?;
        p.snapshot_hash = hash;
        if let Some(j) = p.journal.as_mut() {
            j.reset()?;
        }
        self.last_checkpoint_error = None;
        Ok(())
    }

    /// Alias of [`Graph::checkpoint`].
    pub fn save(&mut self) -> Result<()> {
        self.checkpoint()
    }

    /// Roll back any open transaction, checkpoint, and release the lock.
    pub fn close(mut self) -> Result<()> {
        if self.tx.is_some() {
            self.rollback()?;
        }
        let pending = self.persistence.as_ref().and_then(|p| p.journal.as_ref()).map_or(0, Journal::pending_bytes);
        if !self.read_only && self.poisoned.is_none() && pending > 0 {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Write the graph as a canonical `.graph` document to `out`.
    pub fn write_snapshot_to(&self, out: &mut dyn Write) -> Result<()> {
        let mut w = BufWriter::new(out);
        format::write(&mut w, &self.graph_id, self.generation, &self.store)
            .and_then(|()| w.flush())
            .map_err(|e| GraphError::io("writing snapshot", e))
    }

    /// Save a copy of the graph to another `.graph` file (atomically).
    pub fn save_as(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let tmp = sibling(path, ".tmp");
        write_snapshot(path, &tmp, &self.graph_id, self.generation, &self.store, true).map(|_| ())
    }

    // ------------------------------------------------------------------
    // Node and edge API
    // ------------------------------------------------------------------

    pub fn add_node(&mut self, node: Node) -> Result<()> {
        self.mutate(|m| m.apply(Op::AddNode { node }))
    }

    /// Insert many nodes atomically.
    pub fn add_nodes(&mut self, nodes: impl IntoIterator<Item = Node>) -> Result<usize> {
        self.mutate(|m| {
            let mut n = 0;
            for node in nodes {
                m.apply(Op::AddNode { node })?;
                n += 1;
            }
            Ok(n)
        })
    }

    pub fn add_edge(&mut self, edge: Edge) -> Result<()> {
        self.mutate(|m| m.apply(Op::AddEdge { edge }))
    }

    /// Insert many edges atomically.
    pub fn add_edges(&mut self, edges: impl IntoIterator<Item = Edge>) -> Result<usize> {
        self.mutate(|m| {
            let mut n = 0;
            for edge in edges {
                m.apply(Op::AddEdge { edge })?;
                n += 1;
            }
            Ok(n)
        })
    }

    /// Apply arbitrary low-level ops atomically.
    pub fn apply_ops(&mut self, ops: impl IntoIterator<Item = Op>) -> Result<()> {
        self.mutate(|m| ops.into_iter().try_for_each(|op| m.apply(op)))
    }

    pub fn get_node(&self, id: &str) -> Option<Node> {
        self.store.node_slot(id).and_then(|s| self.store.node_to_public(s))
    }

    pub fn get_edge(&self, id: &str) -> Option<Edge> {
        self.store.edge_slot(id).and_then(|s| self.store.edge_to_public(s))
    }

    pub fn contains_node(&self, id: &str) -> bool {
        self.store.node_slot(id).is_some()
    }

    pub fn contains_edge(&self, id: &str) -> bool {
        self.store.edge_slot(id).is_some()
    }

    /// Set (or, with `Value::Null`, remove) one node property.
    pub fn set_node_property(&mut self, id: &str, key: &str, value: impl Into<Value>) -> Result<()> {
        let (id, key, value) = (id.to_owned(), key.to_owned(), value.into());
        self.mutate(|m| m.apply(Op::SetNodeProperty { id, key, value }))
    }

    /// Merge properties into a node (`Null` values remove keys).
    pub fn update_node(&mut self, id: &str, properties: Properties) -> Result<()> {
        let id = id.to_owned();
        self.mutate(|m| {
            if m.store().node_slot(&id).is_none() {
                return Err(GraphError::NotFound { kind: EntityKind::Node, id: id.clone() });
            }
            for (key, value) in properties {
                m.apply(Op::SetNodeProperty { id: id.clone(), key, value })?;
            }
            Ok(())
        })
    }

    pub fn set_edge_property(&mut self, id: &str, key: &str, value: impl Into<Value>) -> Result<()> {
        let (id, key, value) = (id.to_owned(), key.to_owned(), value.into());
        self.mutate(|m| m.apply(Op::SetEdgeProperty { id, key, value }))
    }

    pub fn update_edge(&mut self, id: &str, properties: Properties) -> Result<()> {
        let id = id.to_owned();
        self.mutate(|m| {
            if m.store().edge_slot(&id).is_none() {
                return Err(GraphError::NotFound { kind: EntityKind::Edge, id: id.clone() });
            }
            for (key, value) in properties {
                m.apply(Op::SetEdgeProperty { id: id.clone(), key, value })?;
            }
            Ok(())
        })
    }

    pub fn add_label(&mut self, id: &str, label: &str) -> Result<()> {
        let (id, label) = (id.to_owned(), label.to_owned());
        self.mutate(|m| m.apply(Op::AddLabel { id, label }))
    }

    pub fn remove_label(&mut self, id: &str, label: &str) -> Result<()> {
        let (id, label) = (id.to_owned(), label.to_owned());
        self.mutate(|m| m.apply(Op::RemoveLabel { id, label }))
    }

    /// Delete a node. With `detach = false` the node must have no edges.
    /// Returns the number of edges deleted along with it.
    pub fn delete_node(&mut self, id: &str, detach: bool) -> Result<usize> {
        let id = id.to_owned();
        self.mutate(|m| {
            let slot = m
                .store()
                .node_slot(&id)
                .ok_or_else(|| GraphError::NotFound { kind: EntityKind::Node, id: id.clone() })?;
            let mut removed = 0;
            if detach {
                for e in m.store().incident_edges(slot) {
                    let eid = m.store().edge(e).expect("incident edge is live").id.to_string();
                    m.apply(Op::DeleteEdge { id: eid })?;
                    removed += 1;
                }
            }
            m.apply(Op::DeleteNode { id: id.clone() })?;
            Ok(removed)
        })
    }

    pub fn delete_edge(&mut self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.mutate(|m| m.apply(Op::DeleteEdge { id }))
    }

    pub fn set_metadata(&mut self, key: &str, value: impl Into<Value>) -> Result<()> {
        let (key, value) = (key.to_owned(), value.into());
        self.mutate(|m| m.apply(Op::SetMetadata { key, value }))
    }

    /// All nodes in storage order.
    pub fn nodes(&self) -> impl Iterator<Item = Node> + '_ {
        self.store.node_slots_iter().filter_map(|s| self.store.node_to_public(s))
    }

    /// All edges in storage order.
    pub fn edges(&self) -> impl Iterator<Item = Edge> + '_ {
        self.store.edge_slots_iter().filter_map(|s| self.store.edge_to_public(s))
    }

    /// Alias of [`Graph::get_node`].
    pub fn find_node(&self, id: &str) -> Option<Node> {
        self.get_node(id)
    }

    /// Nodes with `label` (if given) whose properties equal all of `properties`.
    /// Uses the same planner (and indexes) as the query language.
    pub fn find_nodes(
        &mut self,
        label: Option<&str>,
        properties: &Properties,
        limit: Option<usize>,
    ) -> Result<Vec<Node>> {
        let mut opts = QueryOptions::new();
        let mut props = Vec::new();
        for (i, (k, v)) in properties.iter().enumerate() {
            let p = format!("p{i}");
            opts.parameters.insert(p.clone(), v.clone());
            props.push((k.clone(), Expr::Param(p)));
        }
        if let Some(l) = limit {
            opts.parameters.insert("__limit".into(), Value::Int(i64::try_from(l).unwrap_or(i64::MAX)));
        }
        let query = Query {
            patterns: vec![Pattern {
                path_var: None,
                shortest: false,
                start: NodePattern {
                    var: Some("n".into()),
                    labels: label.map(str::to_owned).into_iter().collect(),
                    props,
                },
                steps: Vec::new(),
            }],
            where_: None,
            updates: Vec::new(),
            ret: Some(ReturnClause {
                distinct: false,
                star: false,
                items: vec![ReturnItem { expr: Expr::Var("n".into()), name: "n".into() }],
                order_by: Vec::new(),
                skip: None,
                limit: limit.map(|_| Expr::Param("__limit".into())),
            }),
        };
        let stmt = Statement { explain: ExplainMode::None, kind: StatementKind::Query(Box::new(query)) };
        let result = self.execute_statement(&stmt, &opts, QueryStats::default())?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|mut r| match r.pop() {
                Some(ResultValue::Node(n)) => Some(n),
                _ => None,
            })
            .collect())
    }

    /// Edges adjacent to node `id` with the node at the other end.
    pub fn neighbors(&self, id: &str, direction: Direction, edge_type: Option<&str>) -> Result<Vec<(Edge, Node)>> {
        let slot = self
            .store
            .node_slot(id)
            .ok_or_else(|| GraphError::NotFound { kind: EntityKind::Node, id: id.to_owned() })?;
        let types: Option<Vec<u32>> = match edge_type {
            None => None,
            Some(t) => match self.store.types.get(t) {
                Some(tid) => Some(vec![tid]),
                None => return Ok(Vec::new()),
            },
        };
        let mut out = Vec::new();
        let _ = self.store.for_each_adjacent::<()>(slot, direction, types.as_deref(), |e, n| {
            if let (Some(edge), Some(node)) = (self.store.edge_to_public(e), self.store.node_to_public(n)) {
                out.push((edge, node));
            }
            std::ops::ControlFlow::Continue(())
        });
        Ok(out)
    }

    /// Unweighted shortest path (fewest edges) from `source` to `target`.
    pub fn shortest_path(
        &self,
        source: &str,
        target: &str,
        edge_types: &[&str],
        direction: Direction,
        max_depth: u32,
    ) -> Result<Option<GraphPath>> {
        let not_found = |id: &str| GraphError::NotFound { kind: EntityKind::Node, id: id.to_owned() };
        let s = self.store.node_slot(source).ok_or_else(|| not_found(source))?;
        let t = self.store.node_slot(target).ok_or_else(|| not_found(target))?;
        if max_depth > self.config.limits.max_traversal_depth {
            return Err(GraphError::LimitExceeded(format!(
                "max_depth {max_depth} exceeds max_traversal_depth {}",
                self.config.limits.max_traversal_depth
            )));
        }
        let types: Option<Vec<u32>> = if edge_types.is_empty() {
            None
        } else {
            let mut ids: Vec<u32> = edge_types.iter().filter_map(|t| self.store.types.get(t)).collect();
            if ids.is_empty() {
                return Ok(None);
            }
            ids.sort_unstable();
            ids.dedup();
            Some(ids)
        };
        let mut parent: HashMap<Slot, (Slot, Slot)> = HashMap::new();
        let mut frontier = vec![s];
        let mut found = s == t;
        let mut depth = 0;
        while !found && depth < max_depth && !frontier.is_empty() {
            let mut next = Vec::new();
            for &n in &frontier {
                let flow = self.store.for_each_adjacent::<()>(n, direction, types.as_deref(), |e, nbr| {
                    if nbr != s && !parent.contains_key(&nbr) {
                        parent.insert(nbr, (n, e));
                        if nbr == t {
                            return std::ops::ControlFlow::Break(());
                        }
                        next.push(nbr);
                    }
                    std::ops::ControlFlow::Continue(())
                });
                if flow.is_break() {
                    found = true;
                    break;
                }
            }
            frontier = next;
            depth += 1;
        }
        if !found {
            return Ok(None);
        }
        let mut edges = Vec::new();
        let mut cur = t;
        while let Some(&(p, e)) = parent.get(&cur) {
            edges.push(e);
            cur = p;
        }
        edges.reverse();
        let pv = walk_path(&self.store, s, &edges);
        let nodes = pv.nodes.iter().filter_map(|n| self.store.node_to_public(*n)).collect();
        let edges = pv.edges.iter().filter_map(|e| self.store.edge_to_public(*e)).collect();
        Ok(Some(GraphPath { nodes, edges }))
    }

    /// Ranked full-text search over node text indexes. With `property`,
    /// only that property's index is used; otherwise all node text indexes.
    pub fn search_text(&self, query: &str, property: Option<&str>, limit: usize) -> Result<Vec<(Node, f64)>> {
        let indexes: Vec<_> = self
            .store
            .indexes(IndexTarget::Node)
            .iter()
            .filter(|i| i.def.kind == IndexKind::Text && property.is_none_or(|p| i.def.property == p))
            .collect();
        if indexes.is_empty() {
            return Err(GraphError::Index(match property {
                Some(p) => format!("no text index on nodes({p}); run CREATE TEXT INDEX ON NODES({p})"),
                None => "no text indexes on nodes; run CREATE TEXT INDEX ON NODES(<property>)".into(),
            }));
        }
        let mut scores: BTreeMap<Slot, f64> = BTreeMap::new();
        for idx in indexes {
            if let Some(t) = idx.text() {
                for (slot, score) in t.search_ranked(query) {
                    *scores.entry(slot).or_insert(0.0) += score;
                }
            }
        }
        let mut ranked: Vec<(Slot, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(ranked
            .into_iter()
            .take(limit)
            .filter_map(|(s, score)| self.store.node_to_public(s).map(|n| (n, score)))
            .collect())
    }

    // ------------------------------------------------------------------
    // Indexes
    // ------------------------------------------------------------------

    pub fn create_index(&mut self, def: IndexDef) -> Result<()> {
        self.mutate(|m| m.apply(Op::CreateIndex { index: def }))
    }

    pub fn drop_index(&mut self, def: IndexDef) -> Result<()> {
        self.mutate(|m| m.apply(Op::DropIndex { index: def }))
    }

    pub fn indexes(&self) -> Vec<IndexInfo> {
        let mut v: Vec<IndexInfo> = [IndexTarget::Node, IndexTarget::Edge]
            .iter()
            .flat_map(|t| self.store.indexes(*t))
            .map(|i| IndexInfo { def: i.def.clone(), entries: i.entries, distinct_keys: i.distinct_keys() })
            .collect();
        v.sort_by(|a, b| a.def.cmp(&b.def));
        v
    }

    /// Check every derived structure against the records. Empty = consistent.
    pub fn verify(&self) -> Vec<String> {
        self.store.verify()
    }

    /// Rebuild all secondary indexes from the records.
    pub fn rebuild_indexes(&mut self) {
        self.store.rebuild_indexes();
    }

    // ------------------------------------------------------------------
    // Query language
    // ------------------------------------------------------------------

    /// Execute one GHQL statement.
    pub fn query(&mut self, text: &str, opts: &QueryOptions) -> Result<QueryResult> {
        if text.len() > self.config.limits.max_query_bytes {
            return Err(GraphError::LimitExceeded(format!(
                "query is {} bytes; the limit is {}",
                text.len(),
                self.config.limits.max_query_bytes
            )));
        }
        let start = Instant::now();
        let (stmt, hit) = self.cache.get_or_parse(text, self.config.statement_cache_size)?;
        let stats = QueryStats { parse_time: start.elapsed(), statement_cache_hit: hit, ..QueryStats::default() };
        self.execute_statement(&stmt, opts, stats)
    }

    /// Execute a statement with no parameters.
    pub fn run(&mut self, text: &str) -> Result<QueryResult> {
        self.query(text, &QueryOptions::default())
    }

    /// Execute `;`-separated statements, each in its own implicit
    /// transaction unless an explicit transaction is open.
    pub fn execute_script(&mut self, text: &str, opts: &QueryOptions) -> Result<Vec<QueryResult>> {
        if text.len() > self.config.limits.max_query_bytes {
            return Err(GraphError::LimitExceeded("script exceeds max_query_bytes".into()));
        }
        let stmts = parse_script(text)?;
        stmts.iter().map(|s| self.execute_statement(s, opts, QueryStats::default())).collect()
    }

    /// Parse and plan without executing.
    pub fn explain(&mut self, text: &str, opts: &QueryOptions) -> Result<QueryPlan> {
        let mut stmt = parse_statement(text)?;
        stmt.explain = ExplainMode::Explain;
        let r = self.execute_statement(&stmt, opts, QueryStats::default())?;
        Ok(r.plan.unwrap_or_default())
    }

    fn budget(&self, opts: &QueryOptions) -> Budget {
        let l = &self.config.limits;
        Budget::new(
            opts.timeout.or(self.config.default_query_timeout),
            opts.cancellation.clone(),
            opts.max_visits.map_or(l.max_visits, |v| v.min(l.max_visits)),
            opts.max_rows.map_or(l.max_result_rows, |v| v.min(l.max_result_rows)),
            l.max_intermediate_rows,
        )
    }

    fn execute_statement(&mut self, stmt: &Statement, opts: &QueryOptions, stats: QueryStats) -> Result<QueryResult> {
        for (name, value) in &opts.parameters {
            crate::model::validate_value(name, value, &self.config.limits)
                .map_err(|e| GraphError::Parameter(format!("${name}: {e}")))?;
        }
        let executes = stmt.explain != ExplainMode::Explain;
        if executes && stmt.is_mutation() {
            if opts.read_only {
                return Err(GraphError::ReadOnly);
            }
            self.check_writable()?;
        }
        let mut budget = self.budget(opts);
        match &stmt.kind {
            StatementKind::ShowIndexes => {
                let rows = self
                    .indexes()
                    .into_iter()
                    .map(|i| {
                        vec![
                            ResultValue::String(i.def.kind.to_string()),
                            ResultValue::String(i.def.target.to_string()),
                            ResultValue::String(i.def.property.clone()),
                            ResultValue::Int(i.entries as i64),
                            ResultValue::Int(i.distinct_keys as i64),
                        ]
                    })
                    .collect();
                Ok(QueryResult {
                    columns: ["kind", "target", "property", "entries", "distinct_keys"].map(String::from).to_vec(),
                    rows,
                    stats,
                    plan: None,
                })
            }
            StatementKind::CreateIndex(def) | StatementKind::DropIndex(def) => {
                let create = matches!(stmt.kind, StatementKind::CreateIndex(_));
                let plan = QueryPlan {
                    steps: vec![crate::engine::result::PlanStep {
                        operator: if create { "CreateIndex" } else { "DropIndex" }.into(),
                        detail: def.to_string(),
                        estimated_rows: None,
                    }],
                    notes: Vec::new(),
                };
                if executes {
                    let op = if create {
                        Op::CreateIndex { index: def.clone() }
                    } else {
                        Op::DropIndex { index: def.clone() }
                    };
                    self.mutate(|m| m.apply(op))?;
                }
                Ok(QueryResult { columns: Vec::new(), rows: Vec::new(), stats, plan: (!executes).then_some(plan) })
            }
            StatementKind::Query(q) => {
                if executes && stmt.is_mutation() {
                    let mode = stmt.explain;
                    self.mutate(|m| run_query(m, q, mode, opts, &mut budget, stats))
                } else {
                    run_query(&mut ReadOnlyMutator { store: &self.store }, q, stmt.explain, opts, &mut budget, stats)
                }
            }
        }
    }
}

impl Drop for Graph {
    fn drop(&mut self) {
        // Uncommitted changes are discarded; committed ones are in the journal.
        if let Some(tx) = self.tx.take() {
            self.undo_all(tx.undo);
        }
    }
}

/// Atomically write a snapshot file; returns its SHA-256.
fn write_snapshot(
    path: &Path,
    tmp: &Path,
    graph_id: &str,
    generation: u64,
    store: &Store,
    sync: bool,
) -> Result<[u8; 32]> {
    let ctx = |what: &str| format!("{what} {}", tmp.display());
    let file = create_truncate(tmp).map_err(|e| GraphError::io(ctx("creating"), e))?;
    let mut hw = HashingWriter::new(BufWriter::with_capacity(1 << 20, file));
    let written = format::write(&mut hw, graph_id, generation, store).and_then(|()| hw.flush());
    let result = written.and_then(|()| {
        let (buf, hash) = hw.finish();
        let file = buf.into_inner().map_err(|e| e.into_error())?;
        if sync {
            file.sync_all()?;
        }
        drop(file);
        atomic_replace(tmp, path, sync)?;
        Ok(hash)
    });
    match result {
        Ok(hash) => Ok(hash),
        Err(e) => {
            let _ = fsutil::remove_if_exists(tmp);
            Err(GraphError::io(format!("writing snapshot {}", path.display()), e))
        }
    }
}
