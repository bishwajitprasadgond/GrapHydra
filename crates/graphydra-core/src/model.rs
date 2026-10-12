//! Public graph data model: nodes, edges, paths and index definitions.
//!
//! These are plain owned values. They are independent of the storage layout
//! and of the query language.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::config::Limits;
use crate::error::{GraphError, Result};
use crate::value::{Properties, Value, deserialize_properties};

/// Property key reserved for the entity id (`n.id` in queries).
pub const RESERVED_ID_KEY: &str = "id";

/// A graph node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_labels")]
    pub labels: BTreeSet<String>,
    #[serde(default, deserialize_with = "deserialize_properties")]
    pub properties: Properties,
}

impl Node {
    pub fn new(id: impl Into<String>) -> Self {
        Node { id: id.into(), labels: BTreeSet::new(), properties: Properties::new() }
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.labels.insert(label.into());
        self
    }

    #[must_use]
    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    pub fn has_label(&self, label: &str) -> bool {
        self.labels.contains(label)
    }
}

fn is_true(b: &bool) -> bool {
    *b
}

fn default_true() -> bool {
    true
}

/// A relationship between two nodes. Edges are directed unless
/// `directed` is false.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(rename = "type")]
    pub edge_type: String,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub directed: bool,
    #[serde(default, deserialize_with = "deserialize_properties")]
    pub properties: Properties,
}

impl Edge {
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        target: impl Into<String>,
        edge_type: impl Into<String>,
    ) -> Self {
        Edge {
            id: id.into(),
            source: source.into(),
            target: target.into(),
            edge_type: edge_type.into(),
            directed: true,
            properties: Properties::new(),
        }
    }

    #[must_use]
    pub fn undirected(mut self) -> Self {
        self.directed = false;
        self
    }

    #[must_use]
    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }
}

/// An alternating sequence of nodes and edges: `nodes.len() == edges.len() + 1`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Path {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Path {
    /// Number of edges in the path.
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }
}

/// Which records an index covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexTarget {
    Node,
    Edge,
}

/// Index implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexKind {
    /// Equality and `IN` lookups.
    Hash,
    /// Equality, ranges, prefix (`STARTS WITH`) and ordered scans.
    Sorted,
    /// Tokenised full-text search (`text_match`, `search_text`).
    Text,
}

impl fmt::Display for IndexKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IndexKind::Hash => "hash",
            IndexKind::Sorted => "sorted",
            IndexKind::Text => "text",
        })
    }
}

impl fmt::Display for IndexTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IndexTarget::Node => "node",
            IndexTarget::Edge => "edge",
        })
    }
}

/// Definition of a secondary property index. Indexes are global over all
/// nodes (or edges) that have the property, independent of labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexDef {
    pub kind: IndexKind,
    pub target: IndexTarget,
    pub property: String,
}

impl IndexDef {
    pub fn new(kind: IndexKind, target: IndexTarget, property: impl Into<String>) -> Self {
        IndexDef { kind, target, property: property.into() }
    }
}

impl fmt::Display for IndexDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} index on {}s({})", self.kind, self.target, self.property)
    }
}

fn deserialize_labels<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<BTreeSet<String>, D::Error> {
    let list: Vec<String> = Vec::deserialize(d)?;
    let mut set = BTreeSet::new();
    for label in list {
        if !set.insert(label.clone()) {
            return Err(serde::de::Error::custom(format!("duplicate label {label:?}")));
        }
    }
    Ok(set)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

pub(crate) fn validate_id(kind: &str, id: &str, limits: &Limits) -> Result<()> {
    if id.is_empty() {
        return Err(GraphError::validation(format!("{kind} id must not be empty")));
    }
    if id.len() > limits.max_id_bytes {
        return Err(GraphError::validation(format!(
            "{kind} id is {} bytes; the limit is {}",
            id.len(),
            limits.max_id_bytes
        )));
    }
    if id.chars().any(char::is_control) {
        return Err(GraphError::validation(format!("{kind} id {id:?} contains control characters")));
    }
    Ok(())
}

/// Labels, edge types and property keys.
pub(crate) fn validate_name(what: &str, name: &str, limits: &Limits) -> Result<()> {
    if name.is_empty() {
        return Err(GraphError::validation(format!("{what} must not be empty")));
    }
    if name.len() > limits.max_name_bytes {
        return Err(GraphError::validation(format!(
            "{what} {:?}... is longer than {} bytes",
            name.chars().take(32).collect::<String>(),
            limits.max_name_bytes
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(GraphError::validation(format!("{what} {name:?} contains control characters")));
    }
    Ok(())
}

pub(crate) fn validate_value(key: &str, value: &Value, limits: &Limits) -> Result<()> {
    fn walk(key: &str, v: &Value, depth: usize, limits: &Limits) -> Result<()> {
        if depth > limits.max_nesting_depth {
            return Err(GraphError::validation(format!(
                "property {key:?} is nested deeper than {} levels",
                limits.max_nesting_depth
            )));
        }
        match v {
            Value::Float(f) if !f.is_finite() => {
                Err(GraphError::validation(format!("property {key:?} contains a non-finite number")))
            }
            Value::String(s) if s.len() > limits.max_string_bytes => Err(GraphError::validation(format!(
                "property {key:?} has a string of {} bytes; the limit is {}",
                s.len(),
                limits.max_string_bytes
            ))),
            Value::List(items) => items.iter().try_for_each(|i| walk(key, i, depth + 1, limits)),
            Value::Map(map) => map.iter().try_for_each(|(k, i)| {
                if k.len() > limits.max_string_bytes {
                    return Err(GraphError::validation(format!("property {key:?} has an oversized map key")));
                }
                walk(key, i, depth + 1, limits)
            }),
            _ => Ok(()),
        }
    }
    walk(key, value, 0, limits)
}

pub(crate) fn validate_properties(props: &Properties, limits: &Limits) -> Result<()> {
    for (key, value) in props {
        validate_property_key(key, limits)?;
        validate_value(key, value, limits)?;
    }
    Ok(())
}

pub(crate) fn validate_property_key(key: &str, limits: &Limits) -> Result<()> {
    validate_name("property key", key, limits)?;
    if key == RESERVED_ID_KEY {
        return Err(GraphError::validation("property key \"id\" is reserved for the entity id; use a different name"));
    }
    Ok(())
}

pub(crate) fn validate_node(node: &Node, limits: &Limits) -> Result<()> {
    validate_id("node", &node.id, limits)?;
    for label in &node.labels {
        validate_name("label", label, limits)?;
    }
    validate_properties(&node.properties, limits)
}

pub(crate) fn validate_edge(edge: &Edge, limits: &Limits) -> Result<()> {
    validate_id("edge", &edge.id, limits)?;
    validate_name("edge type", &edge.edge_type, limits)?;
    validate_properties(&edge.properties, limits)
}

pub(crate) fn validate_index_def(def: &IndexDef, limits: &Limits) -> Result<()> {
    if def.property == RESERVED_ID_KEY {
        return Err(GraphError::Index("ids are always indexed; no index on \"id\" is needed".into()));
    }
    validate_name("indexed property", &def.property, limits)
}
