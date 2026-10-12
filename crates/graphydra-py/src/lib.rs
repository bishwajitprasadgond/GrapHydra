//! Python bindings for GrapHydra (`graphydra._graphydra`).
//!
//! Locking discipline: the graph mutex is only ever held by pure-Rust code
//! (never while converting to or from Python objects), and long operations
//! release the GIL. This makes the bindings deadlock-free when a graph is
//! shared between Python threads.

// Python-facing signatures mirror keyword arguments one-to-one.
#![allow(clippy::too_many_arguments)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use graphydra_core as core;
use graphydra_core::{
    Config, Direction, Durability, Edge as CoreEdge, GraphError as CoreError, IndexDef, IndexKind, IndexTarget,
    Node as CoreNode, OpenOptions, Path as CorePath, QueryOptions, ResultValue, Value,
};
use pyo3::exceptions::{PyException, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};

// ---------------------------------------------------------------------------
// Exceptions
// ---------------------------------------------------------------------------

pyo3::create_exception!(_graphydra, GraphError, PyException, "Base class of all GrapHydra errors.");
pyo3::create_exception!(
    _graphydra,
    QuerySyntaxError,
    GraphError,
    "The query text is not valid GHQL. Has `line` and `column`."
);
pyo3::create_exception!(
    _graphydra,
    QueryError,
    GraphError,
    "Semantic or runtime error in a query (unknown variable, type error, ...)."
);
pyo3::create_exception!(_graphydra, ParameterError, QueryError, "A query parameter is missing or invalid.");
pyo3::create_exception!(
    _graphydra,
    ValidationError,
    GraphError,
    "Data violates a model rule (ids, labels, value limits)."
);
pyo3::create_exception!(
    _graphydra,
    FormatError,
    GraphError,
    "A .graph or import file is malformed or has an unsupported version."
);
pyo3::create_exception!(_graphydra, NotFoundError, GraphError, "A node, edge or file does not exist.");
pyo3::create_exception!(_graphydra, DuplicateIdError, ValidationError, "A node or edge id already exists.");
pyo3::create_exception!(
    _graphydra,
    IntegrityError,
    ValidationError,
    "Referential integrity violation (dangling edge, node with edges)."
);
pyo3::create_exception!(
    _graphydra,
    ReadOnlyError,
    GraphError,
    "Write attempted on a read-only graph or in read-only query mode."
);
pyo3::create_exception!(_graphydra, LockedError, GraphError, "Another process holds the writer lock.");
pyo3::create_exception!(_graphydra, TransactionError, GraphError, "Invalid transaction state.");
pyo3::create_exception!(_graphydra, QueryTimeoutError, GraphError, "The query exceeded its time limit.");
pyo3::create_exception!(_graphydra, CancelledError, GraphError, "The query was cancelled.");
pyo3::create_exception!(
    _graphydra,
    LimitExceededError,
    GraphError,
    "A resource limit (rows, visits, depth, size) was exceeded."
);
pyo3::create_exception!(
    _graphydra,
    StorageError,
    GraphError,
    "I/O failure, corruption, or external modification of the file."
);
pyo3::create_exception!(_graphydra, IndexDefinitionError, GraphError, "Invalid index operation.");
pyo3::create_exception!(_graphydra, ClosedError, GraphError, "The graph has been closed.");

fn to_py_err(e: CoreError) -> PyErr {
    let msg = e.to_string();
    match e {
        CoreError::Syntax(s) => {
            let err = QuerySyntaxError::new_err(msg);
            Python::attach(|py| {
                let v = err.value(py);
                let _ = v.setattr("line", s.line);
                let _ = v.setattr("column", s.column);
            });
            err
        }
        CoreError::Semantic(_) | CoreError::Type(_) => QueryError::new_err(msg),
        CoreError::Parameter(_) => ParameterError::new_err(msg),
        CoreError::Validation(_) => ValidationError::new_err(msg),
        CoreError::Format(_) | CoreError::UnsupportedVersion { .. } => FormatError::new_err(msg),
        CoreError::NotFound { .. } => NotFoundError::new_err(msg),
        CoreError::Io { ref source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            NotFoundError::new_err(msg)
        }
        CoreError::DuplicateId { .. } => DuplicateIdError::new_err(msg),
        CoreError::DanglingEdge { .. } | CoreError::NodeHasEdges { .. } => IntegrityError::new_err(msg),
        CoreError::ReadOnly => ReadOnlyError::new_err(msg),
        CoreError::Locked { .. } => LockedError::new_err(msg),
        CoreError::Transaction(_) => TransactionError::new_err(msg),
        CoreError::Timeout => QueryTimeoutError::new_err(msg),
        CoreError::Cancelled => CancelledError::new_err(msg),
        CoreError::LimitExceeded(_) => LimitExceededError::new_err(msg),
        CoreError::Index(_) => IndexDefinitionError::new_err(msg),
        CoreError::Closed => ClosedError::new_err(msg),
        CoreError::Io { .. } | CoreError::Corrupt(_) | CoreError::Poisoned(_) | CoreError::ExternalModification(_) => {
            StorageError::new_err(msg)
        }
    }
}

trait IntoPyResult<T> {
    fn py(self) -> PyResult<T>;
}

impl<T> IntoPyResult<T> for core::Result<T> {
    fn py(self) -> PyResult<T> {
        self.map_err(to_py_err)
    }
}

// ---------------------------------------------------------------------------
// Value conversion
// ---------------------------------------------------------------------------

const MAX_PY_DEPTH: usize = 64;

fn to_value(obj: &Bound<'_, PyAny>, depth: usize) -> PyResult<Value> {
    if depth > MAX_PY_DEPTH {
        return Err(PyValueError::new_err("value is nested too deeply"));
    }
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if obj.is_instance_of::<PyBool>() {
        return Ok(Value::Bool(obj.extract()?));
    }
    if obj.is_instance_of::<PyInt>() {
        return obj
            .extract::<i64>()
            .map(Value::Int)
            .map_err(|_| PyValueError::new_err("integer does not fit in 64 bits"));
    }
    if obj.is_instance_of::<PyFloat>() {
        let f: f64 = obj.extract()?;
        if !f.is_finite() {
            return Err(PyValueError::new_err("NaN and infinity are not supported"));
        }
        return Ok(Value::Float(f));
    }
    if obj.is_instance_of::<PyString>() {
        return Ok(Value::String(obj.extract()?));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        return list.iter().map(|i| to_value(&i, depth + 1)).collect::<PyResult<Vec<_>>>().map(Value::List);
    }
    if let Ok(tuple) = obj.cast::<PyTuple>() {
        return tuple.iter().map(|i| to_value(&i, depth + 1)).collect::<PyResult<Vec<_>>>().map(Value::List);
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        return Ok(Value::Map(to_props(dict, depth + 1)?));
    }
    Err(PyTypeError::new_err(format!(
        "unsupported value of type {}; use None, bool, int, float, str, list, tuple or dict",
        obj.get_type().name()?
    )))
}

fn to_props(dict: &Bound<'_, PyDict>, depth: usize) -> PyResult<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for (k, v) in dict.iter() {
        let key: String = k.extract().map_err(|_| PyTypeError::new_err("property keys must be strings"))?;
        out.insert(key, to_value(&v, depth)?);
    }
    Ok(out)
}

fn opt_props(props: Option<&Bound<'_, PyDict>>) -> PyResult<BTreeMap<String, Value>> {
    props.map(|d| to_props(d, 0)).transpose().map(Option::unwrap_or_default)
}

fn value_to_py<'py>(py: Python<'py>, v: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        Value::Null => py.None().into_bound(py),
        Value::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        Value::Int(i) => i.into_pyobject(py)?.into_any(),
        Value::Float(f) => f.into_pyobject(py)?.into_any(),
        Value::String(s) => PyString::new(py, s).into_any(),
        Value::List(items) => {
            PyList::new(py, items.iter().map(|i| value_to_py(py, i)).collect::<PyResult<Vec<_>>>()?)?.into_any()
        }
        Value::Map(m) => props_to_py(py, m)?.into_any(),
    })
}

fn props_to_py<'py>(py: Python<'py>, m: &BTreeMap<String, Value>) -> PyResult<Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    for (k, v) in m {
        d.set_item(k, value_to_py(py, v)?)?;
    }
    Ok(d)
}

fn result_to_py<'py>(py: Python<'py>, v: &ResultValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        ResultValue::Null => py.None().into_bound(py),
        ResultValue::Bool(b) => PyBool::new(py, *b).to_owned().into_any(),
        ResultValue::Int(i) => i.into_pyobject(py)?.into_any(),
        ResultValue::Float(f) => f.into_pyobject(py)?.into_any(),
        ResultValue::String(s) => PyString::new(py, s).into_any(),
        ResultValue::List(items) => {
            PyList::new(py, items.iter().map(|i| result_to_py(py, i)).collect::<PyResult<Vec<_>>>()?)?.into_any()
        }
        ResultValue::Map(m) => {
            let d = PyDict::new(py);
            for (k, v) in m {
                d.set_item(k, result_to_py(py, v)?)?;
            }
            d.into_any()
        }
        ResultValue::Node(n) => Bound::new(py, Node(n.clone()))?.into_any(),
        ResultValue::Edge(e) => Bound::new(py, Edge(e.clone()))?.into_any(),
        ResultValue::Path(p) => Bound::new(py, Path(p.clone()))?.into_any(),
    })
}

fn json_to_py<'py>(py: Python<'py>, v: &impl serde::Serialize) -> PyResult<Bound<'py, PyAny>> {
    let value: Value = serde_json::to_value(v)
        .ok()
        .and_then(|j| serde_json::from_value(j).ok())
        .ok_or_else(|| PyValueError::new_err("internal serialization error"))?;
    value_to_py(py, &value)
}

fn parse_direction(s: &str) -> PyResult<Direction> {
    match s.to_ascii_lowercase().as_str() {
        "out" | "outgoing" => Ok(Direction::Outgoing),
        "in" | "incoming" => Ok(Direction::Incoming),
        "both" | "any" => Ok(Direction::Both),
        other => Err(PyValueError::new_err(format!("direction must be 'out', 'in' or 'both', not {other:?}"))),
    }
}

fn parse_index(property: &str, kind: &str, target: &str) -> PyResult<IndexDef> {
    let kind = match kind.to_ascii_lowercase().as_str() {
        "hash" => IndexKind::Hash,
        "sorted" | "range" | "btree" => IndexKind::Sorted,
        "text" | "fulltext" => IndexKind::Text,
        other => {
            return Err(PyValueError::new_err(format!("index kind must be 'hash', 'sorted' or 'text', not {other:?}")));
        }
    };
    let target = match target.to_ascii_lowercase().as_str() {
        "node" | "nodes" => IndexTarget::Node,
        "edge" | "edges" => IndexTarget::Edge,
        other => return Err(PyValueError::new_err(format!("index target must be 'node' or 'edge', not {other:?}"))),
    };
    Ok(IndexDef::new(kind, target, property))
}

// ---------------------------------------------------------------------------
// Value classes
// ---------------------------------------------------------------------------

/// An immutable snapshot of a node.
#[pyclass(module = "graphydra", frozen, eq, skip_from_py_object)]
#[derive(Clone, PartialEq)]
struct Node(CoreNode);

#[pymethods]
impl Node {
    #[new]
    #[pyo3(signature = (id, labels=None, properties=None))]
    fn new(id: String, labels: Option<Vec<String>>, properties: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        Ok(Node(CoreNode {
            id,
            labels: labels.unwrap_or_default().into_iter().collect(),
            properties: opt_props(properties)?,
        }))
    }

    #[getter]
    fn id(&self) -> &str {
        &self.0.id
    }

    /// Sorted list of labels.
    #[getter]
    fn labels(&self) -> Vec<String> {
        self.0.labels.iter().cloned().collect()
    }

    /// A copy of the properties.
    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        props_to_py(py, &self.0.properties)
    }

    fn __getitem__<'py>(&self, py: Python<'py>, key: &str) -> PyResult<Bound<'py, PyAny>> {
        match self.0.properties.get(key) {
            Some(v) => value_to_py(py, v),
            None if key == "id" => Ok(PyString::new(py, &self.0.id).into_any()),
            None => Err(pyo3::exceptions::PyKeyError::new_err(key.to_owned())),
        }
    }

    #[pyo3(signature = (key, default=None))]
    fn get<'py>(&self, py: Python<'py>, key: &str, default: Option<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        match self.0.properties.get(key) {
            Some(v) => value_to_py(py, v),
            None => Ok(default.unwrap_or_else(|| py.None().into_bound(py))),
        }
    }

    fn has_label(&self, label: &str) -> bool {
        self.0.labels.contains(label)
    }

    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("id", &self.0.id)?;
        d.set_item("labels", self.labels())?;
        d.set_item("properties", self.properties(py)?)?;
        Ok(d)
    }

    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ("node", &self.0.id).hash(&mut h);
        h.finish()
    }

    fn __repr__(&self) -> String {
        let labels: String = self.0.labels.iter().map(|l| format!(":{l}")).collect();
        format!("Node({:?}{labels}, {})", self.0.id, serde_json::to_string(&self.0.properties).unwrap_or_default())
    }
}

/// An immutable snapshot of an edge.
#[pyclass(module = "graphydra", frozen, eq, skip_from_py_object)]
#[derive(Clone, PartialEq)]
struct Edge(CoreEdge);

#[pymethods]
impl Edge {
    #[new]
    #[pyo3(signature = (id, source, target, edge_type, properties=None, directed=true))]
    fn new(
        id: String,
        source: String,
        target: String,
        edge_type: String,
        properties: Option<&Bound<'_, PyDict>>,
        directed: bool,
    ) -> PyResult<Self> {
        Ok(Edge(CoreEdge { id, source, target, edge_type, directed, properties: opt_props(properties)? }))
    }

    #[getter]
    fn id(&self) -> &str {
        &self.0.id
    }

    #[getter]
    fn source(&self) -> &str {
        &self.0.source
    }

    #[getter]
    fn target(&self) -> &str {
        &self.0.target
    }

    #[getter(r#type)]
    fn edge_type(&self) -> &str {
        &self.0.edge_type
    }

    #[getter]
    fn directed(&self) -> bool {
        self.0.directed
    }

    #[getter]
    fn properties<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        props_to_py(py, &self.0.properties)
    }

    fn __getitem__<'py>(&self, py: Python<'py>, key: &str) -> PyResult<Bound<'py, PyAny>> {
        match self.0.properties.get(key) {
            Some(v) => value_to_py(py, v),
            None => Err(pyo3::exceptions::PyKeyError::new_err(key.to_owned())),
        }
    }

    #[pyo3(signature = (key, default=None))]
    fn get<'py>(&self, py: Python<'py>, key: &str, default: Option<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
        match self.0.properties.get(key) {
            Some(v) => value_to_py(py, v),
            None => Ok(default.unwrap_or_else(|| py.None().into_bound(py))),
        }
    }

    fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        d.set_item("id", &self.0.id)?;
        d.set_item("source", &self.0.source)?;
        d.set_item("target", &self.0.target)?;
        d.set_item("type", &self.0.edge_type)?;
        d.set_item("directed", self.0.directed)?;
        d.set_item("properties", self.properties(py)?)?;
        Ok(d)
    }

    fn __hash__(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ("edge", &self.0.id).hash(&mut h);
        h.finish()
    }

    fn __repr__(&self) -> String {
        let arrow = if self.0.directed { "->" } else { "--" };
        format!("Edge({:?}: ({}) -[:{}]{arrow} ({}))", self.0.id, self.0.source, self.0.edge_type, self.0.target)
    }
}

/// A path: `nodes[0] -edges[0]- nodes[1] ...`.
#[pyclass(module = "graphydra", frozen, eq, skip_from_py_object)]
#[derive(Clone, PartialEq)]
struct Path(CorePath);

#[pymethods]
impl Path {
    #[getter]
    fn nodes(&self) -> Vec<Node> {
        self.0.nodes.iter().cloned().map(Node).collect()
    }

    #[getter]
    fn edges(&self) -> Vec<Edge> {
        self.0.edges.iter().cloned().map(Edge).collect()
    }

    /// Number of edges.
    fn __len__(&self) -> usize {
        self.0.edges.len()
    }

    fn __repr__(&self) -> String {
        let ids: Vec<&str> = self.0.nodes.iter().map(|n| n.id.as_str()).collect();
        format!("Path({})", ids.join(" -> "))
    }
}

/// Execution plan with the optimizer's notes.
#[pyclass(module = "graphydra", frozen)]
struct QueryPlan(core::QueryPlan);

#[pymethods]
impl QueryPlan {
    #[getter]
    fn steps<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.0.steps)
    }

    #[getter]
    fn notes(&self) -> Vec<String> {
        self.0.notes.clone()
    }

    #[getter]
    fn text(&self) -> String {
        self.0.to_text()
    }

    fn __str__(&self) -> String {
        self.0.to_text()
    }

    fn __repr__(&self) -> String {
        format!("QueryPlan({} steps)", self.0.steps.len())
    }
}

/// Result of a query. Rows are converted to Python objects lazily.
#[pyclass(module = "graphydra", frozen, sequence)]
struct QueryResult {
    inner: core::QueryResult,
}

impl QueryResult {
    fn record<'py>(&self, py: Python<'py>, i: usize) -> PyResult<Bound<'py, PyDict>> {
        let d = PyDict::new(py);
        for (c, v) in self.inner.columns.iter().zip(&self.inner.rows[i]) {
            d.set_item(c, result_to_py(py, v)?)?;
        }
        Ok(d)
    }
}

#[pymethods]
impl QueryResult {
    #[getter]
    fn columns(&self) -> Vec<String> {
        self.inner.columns.clone()
    }

    /// All rows as tuples.
    #[getter]
    fn rows<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyTuple>>> {
        self.inner
            .rows
            .iter()
            .map(|r| PyTuple::new(py, r.iter().map(|v| result_to_py(py, v)).collect::<PyResult<Vec<_>>>()?))
            .collect()
    }

    /// Execution statistics (times in microseconds).
    #[getter]
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        json_to_py(py, &self.inner.stats)
    }

    #[getter]
    fn plan(&self) -> Option<QueryPlan> {
        self.inner.plan.clone().map(QueryPlan)
    }

    fn __len__(&self) -> usize {
        self.inner.rows.len()
    }

    fn __getitem__<'py>(&self, py: Python<'py>, index: isize) -> PyResult<Bound<'py, PyDict>> {
        let n = self.inner.rows.len() as isize;
        let i = if index < 0 { n + index } else { index };
        if !(0..n).contains(&i) {
            return Err(pyo3::exceptions::PyIndexError::new_err("result row index out of range"));
        }
        self.record(py, i as usize)
    }

    fn __iter__(slf: Bound<'_, Self>) -> RowIterator {
        RowIterator { result: slf.unbind(), pos: 0 }
    }

    /// Rows as a list of dicts.
    fn to_list<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        (0..self.inner.rows.len()).map(|i| self.record(py, i)).collect()
    }

    /// Values of one column.
    fn column<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let c = self
            .inner
            .columns
            .iter()
            .position(|n| n == name)
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(name.to_owned()))?;
        self.inner.rows.iter().map(|r| result_to_py(py, &r[c])).collect()
    }

    /// First value of the first row, or None.
    fn scalar<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        match self.inner.rows.first().and_then(|r| r.first()) {
            Some(v) => result_to_py(py, v),
            None => Ok(py.None().into_bound(py)),
        }
    }

    /// The result as JSON text (nodes/edges tagged with "kind").
    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner).map_err(|e| PyValueError::new_err(e.to_string()))
    }

    fn __repr__(&self) -> String {
        format!("QueryResult(columns={:?}, rows={})", self.inner.columns, self.inner.rows.len())
    }
}

#[pyclass(module = "graphydra")]
struct RowIterator {
    result: Py<QueryResult>,
    pos: usize,
}

#[pymethods]
impl RowIterator {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__<'py>(&mut self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let r = self.result.get();
        if self.pos >= r.inner.rows.len() {
            return Ok(None);
        }
        let rec = r.record(py, self.pos)?;
        self.pos += 1;
        Ok(Some(rec))
    }
}

/// Cooperative cancellation token for long queries (thread-safe).
#[pyclass(module = "graphydra", frozen)]
struct CancelToken(core::CancellationToken);

#[pymethods]
impl CancelToken {
    #[new]
    fn new() -> Self {
        CancelToken(core::CancellationToken::new())
    }

    fn cancel(&self) {
        self.0.cancel();
    }

    #[getter]
    fn cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

/// An embedded graph database backed by a `.graph` file (or in memory).
#[pyclass(module = "graphydra", frozen)]
struct Graph {
    inner: Mutex<Option<core::Graph>>,
}

impl Graph {
    fn lock(&self) -> PyResult<MutexGuard<'_, Option<core::Graph>>> {
        // A panic while the lock was held cannot leave the core graph in a
        // torn state (every mutation is all-or-nothing), so recover the guard.
        Ok(self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn with<T>(&self, f: impl FnOnce(&mut core::Graph) -> core::Result<T>) -> PyResult<T> {
        let mut guard = self.lock()?;
        let g = guard.as_mut().ok_or_else(|| to_py_err(CoreError::Closed))?;
        f(g).py()
    }

    fn with_ref<T>(&self, f: impl FnOnce(&core::Graph) -> T) -> PyResult<T> {
        let guard = self.lock()?;
        let g = guard.as_ref().ok_or_else(|| to_py_err(CoreError::Closed))?;
        Ok(f(g))
    }

    fn wrap(g: core::Graph) -> Self {
        Graph { inner: Mutex::new(Some(g)) }
    }
}

fn node_from_py(obj: &Bound<'_, PyAny>) -> PyResult<CoreNode> {
    if let Ok(n) = obj.cast::<Node>() {
        return Ok(n.get().0.clone());
    }
    let d = obj
        .cast::<PyDict>()
        .map_err(|_| PyTypeError::new_err("expected a Node or a dict with 'id', 'labels', 'properties'"))?;
    let id: String = d.get_item("id")?.ok_or_else(|| PyValueError::new_err("node dict needs an 'id'"))?.extract()?;
    let labels: Vec<String> = match d.get_item("labels")? {
        Some(l) if !l.is_none() => l.extract()?,
        _ => Vec::new(),
    };
    let properties = match d.get_item("properties")? {
        Some(p) if !p.is_none() => {
            to_props(p.cast::<PyDict>().map_err(|_| PyTypeError::new_err("'properties' must be a dict"))?, 0)?
        }
        _ => BTreeMap::new(),
    };
    Ok(CoreNode { id, labels: labels.into_iter().collect(), properties })
}

fn edge_from_py(obj: &Bound<'_, PyAny>) -> PyResult<CoreEdge> {
    if let Ok(e) = obj.cast::<Edge>() {
        return Ok(e.get().0.clone());
    }
    let d = obj.cast::<PyDict>().map_err(|_| PyTypeError::new_err("expected an Edge or a dict"))?;
    let get = |k: &str| -> PyResult<String> {
        d.get_item(k)?.ok_or_else(|| PyValueError::new_err(format!("edge dict needs {k:?}")))?.extract()
    };
    let properties = match d.get_item("properties")? {
        Some(p) if !p.is_none() => {
            to_props(p.cast::<PyDict>().map_err(|_| PyTypeError::new_err("'properties' must be a dict"))?, 0)?
        }
        _ => BTreeMap::new(),
    };
    let directed = match d.get_item("directed")? {
        Some(v) if !v.is_none() => v.extract()?,
        _ => true,
    };
    let edge_type = match d.get_item("type")? {
        Some(t) => t.extract()?,
        None => get("edge_type")?,
    };
    Ok(CoreEdge { id: get("id")?, source: get("source")?, target: get("target")?, edge_type, directed, properties })
}

fn query_options(
    parameters: Option<&Bound<'_, PyDict>>,
    timeout: Option<f64>,
    read_only: bool,
    max_rows: Option<usize>,
    cancel: Option<&Bound<'_, CancelToken>>,
) -> PyResult<QueryOptions> {
    let mut opts = QueryOptions::new();
    if let Some(p) = parameters {
        opts.parameters = to_props(p, 0)?;
    }
    if let Some(t) = timeout {
        opts.timeout = Some(
            Duration::try_from_secs_f64(t)
                .map_err(|_| PyValueError::new_err("timeout must be a positive number of seconds"))?,
        );
    }
    opts.read_only = read_only;
    opts.max_rows = max_rows;
    opts.cancellation = cancel.map(|c| c.get().0.clone());
    Ok(opts)
}

#[pymethods]
impl Graph {
    /// Open (or with ``create=True`` create) a ``.graph`` file.
    #[staticmethod]
    #[pyo3(signature = (path, *, create=false, read_only=false, graph_id=None, durability="full", default_timeout=None))]
    fn open(
        py: Python<'_>,
        path: PathBuf,
        create: bool,
        read_only: bool,
        graph_id: Option<String>,
        durability: &str,
        default_timeout: Option<f64>,
    ) -> PyResult<Self> {
        let durability = match durability {
            "full" => Durability::Full,
            "nosync" | "none" => Durability::NoSync,
            other => {
                return Err(PyValueError::new_err(format!("durability must be 'full' or 'nosync', not {other:?}")));
            }
        };
        let mut config = Config { durability, ..Config::default() };
        if let Some(t) = default_timeout {
            config.default_query_timeout =
                Some(Duration::try_from_secs_f64(t).map_err(|_| PyValueError::new_err("invalid timeout"))?);
        }
        let mut opts = OpenOptions::new().create(create).read_only(read_only).config(config);
        opts.graph_id = graph_id;
        let g = py.detach(|| core::Graph::open_with(&path, opts)).py()?;
        Ok(Graph::wrap(g))
    }

    /// Create a new file; fails if it exists.
    #[staticmethod]
    fn create(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        py.detach(|| core::Graph::create(&path)).py().map(Graph::wrap)
    }

    /// A non-persistent graph.
    #[staticmethod]
    fn in_memory() -> Self {
        Graph::wrap(core::Graph::in_memory())
    }

    #[getter]
    fn graph_id(&self) -> PyResult<String> {
        self.with_ref(|g| g.graph_id().to_owned())
    }

    #[getter]
    fn path(&self) -> PyResult<Option<PathBuf>> {
        self.with_ref(|g| g.path().map(std::path::Path::to_path_buf))
    }

    #[getter]
    fn generation(&self) -> PyResult<u64> {
        self.with_ref(core::Graph::generation)
    }

    #[getter]
    fn node_count(&self) -> PyResult<usize> {
        self.with_ref(core::Graph::node_count)
    }

    #[getter]
    fn edge_count(&self) -> PyResult<usize> {
        self.with_ref(core::Graph::edge_count)
    }

    #[getter]
    fn read_only(&self) -> PyResult<bool> {
        self.with_ref(core::Graph::is_read_only)
    }

    #[getter]
    fn in_transaction(&self) -> PyResult<bool> {
        self.with_ref(core::Graph::in_transaction)
    }

    #[getter]
    fn closed(&self) -> PyResult<bool> {
        Ok(self.lock()?.is_none())
    }

    #[getter]
    fn metadata<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let m = self.with_ref(|g| g.metadata().clone())?;
        props_to_py(py, &m)
    }

    fn set_metadata(&self, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let v = to_value(value, 0)?;
        self.with(|g| g.set_metadata(key, v))
    }

    fn __len__(&self) -> PyResult<usize> {
        self.node_count()
    }

    fn __contains__(&self, id: &str) -> PyResult<bool> {
        self.with_ref(|g| g.contains_node(id))
    }

    fn __repr__(&self) -> String {
        match self.with_ref(|g| (g.graph_id().to_owned(), g.node_count(), g.edge_count())) {
            Ok((id, n, e)) => format!("Graph({id:?}, nodes={n}, edges={e})"),
            Err(_) => "Graph(<closed>)".into(),
        }
    }

    // --- nodes and edges -------------------------------------------------

    #[pyo3(signature = (id, labels=None, properties=None))]
    fn add_node(
        &self,
        id: String,
        labels: Option<Vec<String>>,
        properties: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Node> {
        let node = CoreNode {
            id,
            labels: labels.unwrap_or_default().into_iter().collect(),
            properties: opt_props(properties)?,
        };
        let out = node.clone();
        self.with(|g| g.add_node(node))?;
        Ok(Node(out))
    }

    /// Insert many nodes (Node objects or dicts) atomically. Returns the count.
    fn add_nodes(&self, py: Python<'_>, nodes: &Bound<'_, PyAny>) -> PyResult<usize> {
        let list = nodes.try_iter()?.map(|n| node_from_py(&n?)).collect::<PyResult<Vec<_>>>()?;
        py.detach(|| self.with(|g| g.add_nodes(list)))
    }

    #[pyo3(signature = (id, source, target, edge_type, properties=None, directed=true))]
    fn add_edge(
        &self,
        id: String,
        source: String,
        target: String,
        edge_type: String,
        properties: Option<&Bound<'_, PyDict>>,
        directed: bool,
    ) -> PyResult<Edge> {
        let edge = CoreEdge { id, source, target, edge_type, directed, properties: opt_props(properties)? };
        let out = edge.clone();
        self.with(|g| g.add_edge(edge))?;
        Ok(Edge(out))
    }

    /// Insert many edges (Edge objects or dicts) atomically. Returns the count.
    fn add_edges(&self, py: Python<'_>, edges: &Bound<'_, PyAny>) -> PyResult<usize> {
        let list = edges.try_iter()?.map(|e| edge_from_py(&e?)).collect::<PyResult<Vec<_>>>()?;
        py.detach(|| self.with(|g| g.add_edges(list)))
    }

    fn get_node(&self, id: &str) -> PyResult<Option<Node>> {
        self.with_ref(|g| g.get_node(id).map(Node))
    }

    /// Alias of ``get_node``.
    fn find_node(&self, id: &str) -> PyResult<Option<Node>> {
        self.get_node(id)
    }

    fn get_edge(&self, id: &str) -> PyResult<Option<Edge>> {
        self.with_ref(|g| g.get_edge(id).map(Edge))
    }

    /// Nodes with ``label`` whose properties equal all of ``properties``.
    #[pyo3(signature = (label=None, properties=None, limit=None))]
    fn find_nodes(
        &self,
        py: Python<'_>,
        label: Option<String>,
        properties: Option<&Bound<'_, PyDict>>,
        limit: Option<usize>,
    ) -> PyResult<Vec<Node>> {
        let props = opt_props(properties)?;
        let nodes = py.detach(|| self.with(|g| g.find_nodes(label.as_deref(), &props, limit)))?;
        Ok(nodes.into_iter().map(Node).collect())
    }

    /// ``(edge, neighbor)`` pairs. ``direction`` is 'out', 'in' or 'both'.
    #[pyo3(signature = (id, direction="out", edge_type=None))]
    fn neighbors(&self, id: &str, direction: &str, edge_type: Option<&str>) -> PyResult<Vec<(Edge, Node)>> {
        let dir = parse_direction(direction)?;
        let pairs = self.with(|g| g.neighbors(id, dir, edge_type))?;
        Ok(pairs.into_iter().map(|(e, n)| (Edge(e), Node(n))).collect())
    }

    fn set_node_property(&self, id: &str, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let v = to_value(value, 0)?;
        self.with(|g| g.set_node_property(id, key, v))
    }

    /// Merge properties into a node; ``None`` removes a key.
    fn update_node(&self, id: &str, properties: &Bound<'_, PyDict>) -> PyResult<()> {
        let props = to_props(properties, 0)?;
        self.with(|g| g.update_node(id, props))
    }

    fn set_edge_property(&self, id: &str, key: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let v = to_value(value, 0)?;
        self.with(|g| g.set_edge_property(id, key, v))
    }

    fn update_edge(&self, id: &str, properties: &Bound<'_, PyDict>) -> PyResult<()> {
        let props = to_props(properties, 0)?;
        self.with(|g| g.update_edge(id, props))
    }

    fn add_label(&self, id: &str, label: &str) -> PyResult<()> {
        self.with(|g| g.add_label(id, label))
    }

    fn remove_label(&self, id: &str, label: &str) -> PyResult<()> {
        self.with(|g| g.remove_label(id, label))
    }

    /// Delete a node; with ``detach=True`` its edges are deleted too.
    #[pyo3(signature = (id, detach=false))]
    fn delete_node(&self, id: &str, detach: bool) -> PyResult<usize> {
        self.with(|g| g.delete_node(id, detach))
    }

    fn delete_edge(&self, id: &str) -> PyResult<()> {
        self.with(|g| g.delete_edge(id))
    }

    /// All nodes (snapshot list).
    fn nodes(&self) -> PyResult<Vec<Node>> {
        self.with_ref(|g| g.nodes().map(Node).collect())
    }

    /// All edges (snapshot list).
    fn edges(&self) -> PyResult<Vec<Edge>> {
        self.with_ref(|g| g.edges().map(Edge).collect())
    }

    #[pyo3(signature = (source, target, *, edge_types=None, direction="both", max_depth=16))]
    fn shortest_path(
        &self,
        py: Python<'_>,
        source: &str,
        target: &str,
        edge_types: Option<Vec<String>>,
        direction: &str,
        max_depth: u32,
    ) -> PyResult<Option<Path>> {
        let dir = parse_direction(direction)?;
        let types = edge_types.unwrap_or_default();
        py.detach(|| {
            self.with(|g| {
                let refs: Vec<&str> = types.iter().map(String::as_str).collect();
                g.shortest_path(source, target, &refs, dir, max_depth)
            })
        })
        .map(|p| p.map(Path))
    }

    /// Ranked full-text search over node text indexes: ``[(node, score)]``.
    #[pyo3(signature = (query, *, property=None, limit=10))]
    fn search_text(
        &self,
        py: Python<'_>,
        query: &str,
        property: Option<&str>,
        limit: usize,
    ) -> PyResult<Vec<(Node, f64)>> {
        let hits = py.detach(|| self.with(|g| g.search_text(query, property, limit)))?;
        Ok(hits.into_iter().map(|(n, s)| (Node(n), s)).collect())
    }

    // --- query language --------------------------------------------------

    /// Execute one GHQL statement. Use ``parameters`` instead of string
    /// formatting for untrusted values.
    #[pyo3(signature = (text, parameters=None, *, timeout=None, read_only=false, max_rows=None, cancel=None))]
    fn query(
        &self,
        py: Python<'_>,
        text: &str,
        parameters: Option<&Bound<'_, PyDict>>,
        timeout: Option<f64>,
        read_only: bool,
        max_rows: Option<usize>,
        cancel: Option<&Bound<'_, CancelToken>>,
    ) -> PyResult<QueryResult> {
        let opts = query_options(parameters, timeout, read_only, max_rows, cancel)?;
        let inner = py.detach(|| self.with(|g| g.query(text, &opts)))?;
        Ok(QueryResult { inner })
    }

    /// Execute ``;``-separated statements; returns one result per statement.
    #[pyo3(signature = (text, parameters=None, *, timeout=None, read_only=false))]
    fn execute_script(
        &self,
        py: Python<'_>,
        text: &str,
        parameters: Option<&Bound<'_, PyDict>>,
        timeout: Option<f64>,
        read_only: bool,
    ) -> PyResult<Vec<QueryResult>> {
        let opts = query_options(parameters, timeout, read_only, None, None)?;
        let results = py.detach(|| self.with(|g| g.execute_script(text, &opts)))?;
        Ok(results.into_iter().map(|inner| QueryResult { inner }).collect())
    }

    /// Plan a statement without executing it.
    #[pyo3(signature = (text, parameters=None))]
    fn explain(&self, py: Python<'_>, text: &str, parameters: Option<&Bound<'_, PyDict>>) -> PyResult<QueryPlan> {
        let opts = query_options(parameters, None, false, None, None)?;
        py.detach(|| self.with(|g| g.explain(text, &opts))).map(QueryPlan)
    }

    // --- indexes ----------------------------------------------------------

    #[pyo3(signature = (property, *, kind="hash", target="node"))]
    fn create_index(&self, py: Python<'_>, property: &str, kind: &str, target: &str) -> PyResult<()> {
        let def = parse_index(property, kind, target)?;
        py.detach(|| self.with(|g| g.create_index(def)))
    }

    #[pyo3(signature = (property, *, kind="hash", target="node"))]
    fn drop_index(&self, property: &str, kind: &str, target: &str) -> PyResult<()> {
        let def = parse_index(property, kind, target)?;
        self.with(|g| g.drop_index(def))
    }

    fn indexes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let idx = self.with_ref(core::Graph::indexes)?;
        json_to_py(py, &idx)
    }

    /// Consistency problems between records and indexes (empty = OK).
    fn verify(&self, py: Python<'_>) -> PyResult<Vec<String>> {
        py.detach(|| self.with_ref(core::Graph::verify))
    }

    fn rebuild_indexes(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.with(|g| {
                g.rebuild_indexes();
                Ok(())
            })
        })
    }

    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let s = self.with_ref(core::Graph::stats)?;
        json_to_py(py, &s)
    }

    // --- transactions and persistence ---------------------------------------

    fn begin(&self) -> PyResult<()> {
        self.with(core::Graph::begin)
    }

    fn commit(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.with(core::Graph::commit))
    }

    fn rollback(&self) -> PyResult<()> {
        self.with(core::Graph::rollback)
    }

    /// ``with graph.transaction(): ...`` commits on success, rolls back on error.
    fn transaction(slf: Bound<'_, Self>) -> Transaction {
        Transaction { graph: slf.unbind() }
    }

    /// Write a fresh snapshot and truncate the journal.
    fn checkpoint(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.with(core::Graph::checkpoint))
    }

    /// Alias of ``checkpoint``.
    fn save(&self, py: Python<'_>) -> PyResult<()> {
        self.checkpoint(py)
    }

    /// Save a copy to another ``.graph`` file.
    fn save_as(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        py.detach(|| self.with(|g| g.save_as(&path)))
    }

    /// Roll back any open transaction, checkpoint and release the lock.
    /// Further use raises ``ClosedError``. Closing twice is a no-op.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let g = self.lock()?.take();
        match g {
            Some(g) => py.detach(|| g.close()).py(),
            None => Ok(()),
        }
    }

    fn __enter__(slf: Bound<'_, Self>) -> Bound<'_, Self> {
        slf
    }

    #[pyo3(signature = (*_args))]
    fn __exit__(&self, py: Python<'_>, _args: &Bound<'_, PyTuple>) -> PyResult<bool> {
        self.close(py)?;
        Ok(false)
    }

    // --- import / export ------------------------------------------------------

    fn import_json(&self, py: Python<'_>, path: PathBuf) -> PyResult<(usize, usize)> {
        py.detach(|| {
            self.with(|g| {
                let f = std::fs::File::open(&path)
                    .map_err(|e| CoreError::Io { context: format!("opening {}", path.display()), source: e })?;
                core::interop::import_json(g, std::io::BufReader::new(f)).map(|r| (r.nodes, r.edges))
            })
        })
    }

    fn import_jsonl(&self, py: Python<'_>, path: PathBuf) -> PyResult<(usize, usize)> {
        py.detach(|| {
            self.with(|g| {
                let f = std::fs::File::open(&path)
                    .map_err(|e| CoreError::Io { context: format!("opening {}", path.display()), source: e })?;
                core::interop::import_jsonl(g, std::io::BufReader::new(f)).map(|r| (r.nodes, r.edges))
            })
        })
    }

    #[pyo3(signature = (nodes=None, edges=None))]
    fn import_csv(&self, py: Python<'_>, nodes: Option<PathBuf>, edges: Option<PathBuf>) -> PyResult<(usize, usize)> {
        py.detach(|| {
            self.with(|g| {
                let open = |p: &Option<PathBuf>| -> core::Result<Option<std::fs::File>> {
                    p.as_ref()
                        .map(|p| {
                            std::fs::File::open(p)
                                .map_err(|e| CoreError::Io { context: format!("opening {}", p.display()), source: e })
                        })
                        .transpose()
                };
                core::interop::import_csv(g, open(&nodes)?, open(&edges)?).map(|r| (r.nodes, r.edges))
            })
        })
    }

    /// Export as ``json``, ``jsonl`` or ``graphml``.
    #[pyo3(signature = (path, format="json"))]
    fn export(&self, py: Python<'_>, path: PathBuf, format: &str) -> PyResult<()> {
        let fmt = format.to_ascii_lowercase();
        if !matches!(fmt.as_str(), "json" | "jsonl" | "graphml") {
            return Err(PyValueError::new_err("format must be 'json', 'jsonl' or 'graphml' (use export_csv for CSV)"));
        }
        py.detach(|| {
            self.with(|g| {
                let mut f = std::fs::File::create(&path)
                    .map_err(|e| CoreError::Io { context: format!("creating {}", path.display()), source: e })?;
                match fmt.as_str() {
                    "json" => core::interop::export_json(g, &mut f),
                    "jsonl" => core::interop::export_jsonl(g, &mut f),
                    _ => core::interop::export_graphml(g, &mut f),
                }
            })
        })
    }

    fn export_csv(&self, py: Python<'_>, nodes_path: PathBuf, edges_path: PathBuf) -> PyResult<()> {
        py.detach(|| {
            self.with(|g| {
                let create = |p: &PathBuf| {
                    std::fs::File::create(p)
                        .map_err(|e| CoreError::Io { context: format!("creating {}", p.display()), source: e })
                };
                core::interop::export_csv(g, &mut create(&nodes_path)?, &mut create(&edges_path)?)
            })
        })
    }
}

/// Context manager returned by ``Graph.transaction()``.
#[pyclass(module = "graphydra")]
struct Transaction {
    graph: Py<Graph>,
}

#[pymethods]
impl Transaction {
    fn __enter__(slf: PyRef<'_, Self>, py: Python<'_>) -> PyResult<Py<Graph>> {
        slf.graph.get().begin()?;
        Ok(slf.graph.clone_ref(py))
    }

    #[pyo3(signature = (exc_type=None, _exc=None, _tb=None))]
    fn __exit__(
        &self,
        py: Python<'_>,
        exc_type: Option<&Bound<'_, PyAny>>,
        _exc: Option<&Bound<'_, PyAny>>,
        _tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        let g = self.graph.get();
        if exc_type.is_some_and(|t| !t.is_none()) {
            if g.in_transaction()? {
                g.rollback()?;
            }
        } else {
            g.commit(py)?;
        }
        Ok(false)
    }
}

/// Validate a ``.graph`` file without modifying it; returns a report dict.
#[pyfunction]
fn validate<'py>(py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
    let report = py.detach(|| core::validate_file(&path));
    json_to_py(py, &report)
}

/// Split text into the terms used by text indexes.
#[pyfunction]
fn tokenize(text: &str) -> Vec<String> {
    core::indexes::text::tokenize(text)
}

#[pymodule]
fn _graphydra(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    m.add("__version__", core::VERSION)?;
    m.add("FORMAT_VERSION", core::storage::FORMAT_VERSION)?;
    m.add_class::<Graph>()?;
    m.add_class::<Node>()?;
    m.add_class::<Edge>()?;
    m.add_class::<Path>()?;
    m.add_class::<QueryResult>()?;
    m.add_class::<QueryPlan>()?;
    m.add_class::<CancelToken>()?;
    m.add_class::<Transaction>()?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    m.add_function(wrap_pyfunction!(tokenize, m)?)?;
    for (name, ty) in [
        ("GraphError", py.get_type::<GraphError>()),
        ("QuerySyntaxError", py.get_type::<QuerySyntaxError>()),
        ("QueryError", py.get_type::<QueryError>()),
        ("ParameterError", py.get_type::<ParameterError>()),
        ("ValidationError", py.get_type::<ValidationError>()),
        ("FormatError", py.get_type::<FormatError>()),
        ("NotFoundError", py.get_type::<NotFoundError>()),
        ("DuplicateIdError", py.get_type::<DuplicateIdError>()),
        ("IntegrityError", py.get_type::<IntegrityError>()),
        ("ReadOnlyError", py.get_type::<ReadOnlyError>()),
        ("LockedError", py.get_type::<LockedError>()),
        ("TransactionError", py.get_type::<TransactionError>()),
        ("QueryTimeoutError", py.get_type::<QueryTimeoutError>()),
        ("CancelledError", py.get_type::<CancelledError>()),
        ("LimitExceededError", py.get_type::<LimitExceededError>()),
        ("StorageError", py.get_type::<StorageError>()),
        ("IndexDefinitionError", py.get_type::<IndexDefinitionError>()),
        ("ClosedError", py.get_type::<ClosedError>()),
    ] {
        m.add(name, ty)?;
    }
    Ok(())
}
