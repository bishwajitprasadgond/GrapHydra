//! Public query options, results, statistics and plans.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::ser::SerializeMap;

use crate::error::{GraphError, Result};
use crate::model::{Edge, Node, Path};
use crate::value::Value;

/// A value in a query result.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<ResultValue>),
    Map(BTreeMap<String, ResultValue>),
    Node(Node),
    Edge(Edge),
    Path(Path),
}

impl ResultValue {
    /// Convert to a plain property value, if the result holds no entities.
    pub fn as_value(&self) -> Option<Value> {
        Some(match self {
            ResultValue::Null => Value::Null,
            ResultValue::Bool(b) => Value::Bool(*b),
            ResultValue::Int(i) => Value::Int(*i),
            ResultValue::Float(f) => Value::Float(*f),
            ResultValue::String(s) => Value::String(s.clone()),
            ResultValue::List(l) => Value::List(l.iter().map(ResultValue::as_value).collect::<Option<_>>()?),
            ResultValue::Map(m) => {
                Value::Map(m.iter().map(|(k, v)| Some((k.clone(), v.as_value()?))).collect::<Option<_>>()?)
            }
            ResultValue::Node(_) | ResultValue::Edge(_) | ResultValue::Path(_) => return None,
        })
    }

    pub fn as_node(&self) -> Option<&Node> {
        match self {
            ResultValue::Node(n) => Some(n),
            _ => None,
        }
    }
}

impl From<Value> for ResultValue {
    fn from(v: Value) -> Self {
        match v {
            Value::Null => ResultValue::Null,
            Value::Bool(b) => ResultValue::Bool(b),
            Value::Int(i) => ResultValue::Int(i),
            Value::Float(f) => ResultValue::Float(f),
            Value::String(s) => ResultValue::String(s),
            Value::List(l) => ResultValue::List(l.into_iter().map(Into::into).collect()),
            Value::Map(m) => ResultValue::Map(m.into_iter().map(|(k, v)| (k, v.into())).collect()),
        }
    }
}

/// JSON form: plain JSON for data values; entities become objects tagged
/// with `"kind": "node" | "edge" | "path"`.
impl Serialize for ResultValue {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            ResultValue::Null => s.serialize_unit(),
            ResultValue::Bool(b) => s.serialize_bool(*b),
            ResultValue::Int(i) => s.serialize_i64(*i),
            ResultValue::Float(f) => s.serialize_f64(*f),
            ResultValue::String(v) => s.serialize_str(v),
            ResultValue::List(l) => l.serialize(s),
            ResultValue::Map(m) => m.serialize(s),
            ResultValue::Node(n) => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("kind", "node")?;
                m.serialize_entry("id", &n.id)?;
                m.serialize_entry("labels", &n.labels)?;
                m.serialize_entry("properties", &n.properties)?;
                m.end()
            }
            ResultValue::Edge(e) => {
                let mut m = s.serialize_map(Some(7))?;
                m.serialize_entry("kind", "edge")?;
                m.serialize_entry("id", &e.id)?;
                m.serialize_entry("type", &e.edge_type)?;
                m.serialize_entry("source", &e.source)?;
                m.serialize_entry("target", &e.target)?;
                m.serialize_entry("directed", &e.directed)?;
                m.serialize_entry("properties", &e.properties)?;
                m.end()
            }
            ResultValue::Path(p) => {
                let nodes: Vec<ResultValue> = p.nodes.iter().cloned().map(ResultValue::Node).collect();
                let edges: Vec<ResultValue> = p.edges.iter().cloned().map(ResultValue::Edge).collect();
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("kind", "path")?;
                m.serialize_entry("nodes", &nodes)?;
                m.serialize_entry("edges", &edges)?;
                m.end()
            }
        }
    }
}

/// Execution counters and timings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueryStats {
    #[serde(serialize_with = "ser_micros")]
    pub parse_time: Duration,
    #[serde(serialize_with = "ser_micros")]
    pub plan_time: Duration,
    #[serde(serialize_with = "ser_micros")]
    pub execution_time: Duration,
    /// Nodes examined by scans and expansions.
    pub nodes_scanned: u64,
    /// Edges examined during expansion.
    pub edges_traversed: u64,
    /// Primary or secondary index lookups performed.
    pub index_lookups: u64,
    pub rows_returned: u64,
    pub statement_cache_hit: bool,
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub edges_created: u64,
    pub edges_deleted: u64,
    pub properties_set: u64,
    pub labels_added: u64,
    pub labels_removed: u64,
}

fn ser_micros<S: serde::Serializer>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_f64(d.as_secs_f64() * 1e6)
}

impl QueryStats {
    pub fn contains_updates(&self) -> bool {
        self.nodes_created
            + self.nodes_deleted
            + self.edges_created
            + self.edges_deleted
            + self.properties_set
            + self.labels_added
            + self.labels_removed
            > 0
    }
}

/// One operator of an execution plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanStep {
    pub operator: String,
    pub detail: String,
    /// Estimated rows produced per input row (for scans: total).
    pub estimated_rows: Option<u64>,
}

/// The selected execution plan with the optimizer's reasoning.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueryPlan {
    pub steps: Vec<PlanStep>,
    /// Access-path decisions and hints, e.g. why an index was (not) used.
    pub notes: Vec<String>,
}

impl QueryPlan {
    /// Human-readable rendering.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.steps.iter().enumerate() {
            let est = s.estimated_rows.map(|e| format!("  (est. {e})")).unwrap_or_default();
            out.push_str(&format!("{:>2}. {:<16} {}{}\n", i + 1, s.operator, s.detail, est));
        }
        for n in &self.notes {
            out.push_str(&format!("note: {n}\n"));
        }
        out
    }
}

/// Result of executing a statement.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ResultValue>>,
    pub stats: QueryStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<QueryPlan>,
}

impl QueryResult {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Value of `column` in `row`.
    pub fn get(&self, row: usize, column: &str) -> Option<&ResultValue> {
        let c = self.columns.iter().position(|n| n == column)?;
        self.rows.get(row)?.get(c)
    }

    /// Rows as column-name -> value maps.
    pub fn records(&self) -> Vec<BTreeMap<&str, &ResultValue>> {
        self.rows.iter().map(|r| self.columns.iter().map(String::as_str).zip(r.iter()).collect()).collect()
    }
}

/// Cooperative cancellation flag shareable across threads.
#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Per-query execution options.
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    /// Query parameters referenced as `$name`.
    pub parameters: BTreeMap<String, Value>,
    /// Abort after this duration (overrides the graph default).
    pub timeout: Option<Duration>,
    /// Refuse any statement that could modify the graph.
    pub read_only: bool,
    /// Override `Limits::max_result_rows` (can only lower it).
    pub max_rows: Option<usize>,
    /// Override `Limits::max_visits` (can only lower it).
    pub max_visits: Option<u64>,
    /// Disable ID and property index access paths (for verification:
    /// results must be identical either way).
    pub disable_indexes: bool,
    pub cancellation: Option<CancellationToken>,
}

impl QueryOptions {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn param(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.parameters.insert(name.into(), value.into());
        self
    }

    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}

/// Resource accounting for one execution.
pub(crate) struct Budget {
    deadline: Option<Instant>,
    cancel: Option<CancellationToken>,
    max_visits: u64,
    visits: u64,
    pub(crate) max_rows: usize,
    pub(crate) max_intermediate: usize,
    tick: u32,
}

impl Budget {
    pub(crate) fn new(
        timeout: Option<Duration>,
        cancel: Option<CancellationToken>,
        max_visits: u64,
        max_rows: usize,
        max_intermediate: usize,
    ) -> Self {
        Budget {
            deadline: timeout.map(|t| Instant::now() + t),
            cancel,
            max_visits,
            visits: 0,
            max_rows,
            max_intermediate,
            tick: 0,
        }
    }

    /// Account for `n` visited records; periodically checks time and cancellation.
    #[inline]
    pub(crate) fn visit(&mut self, n: u64) -> Result<()> {
        self.visits += n;
        if self.visits > self.max_visits {
            return Err(GraphError::LimitExceeded(format!(
                "query examined more than {} records (max_visits)",
                self.max_visits
            )));
        }
        self.tick = self.tick.wrapping_add(1);
        if self.tick.is_multiple_of(1024) {
            self.check()?;
        }
        Ok(())
    }

    pub(crate) fn check(&self) -> Result<()> {
        if let Some(c) = &self.cancel
            && c.is_cancelled()
        {
            return Err(GraphError::Cancelled);
        }
        if let Some(d) = self.deadline
            && Instant::now() >= d
        {
            return Err(GraphError::Timeout);
        }
        Ok(())
    }

    pub(crate) fn check_rows(&self, n: usize, what: &str) -> Result<()> {
        let limit = if what == "result" { self.max_rows } else { self.max_intermediate };
        if n > limit {
            return Err(GraphError::LimitExceeded(format!("more than {limit} {what} rows")));
        }
        Ok(())
    }
}
