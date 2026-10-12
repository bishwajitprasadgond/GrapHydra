//! Reader and writer for the canonical `.graph` snapshot (format 1.0).
//!
//! The format is UTF-8 JSON with a fixed top-level schema. Parsing is
//! strict: duplicate keys, unknown fields, out-of-range numbers, dangling
//! edges and duplicate ids are all rejected. Files are data only; nothing in
//! them is ever executed or used to resolve other paths.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Limits;
use crate::error::{GraphError, Result};
use crate::model::{Edge, IndexDef, Node, validate_id, validate_name, validate_value};
use crate::ops::Op;
use crate::store::Store;
use crate::value::{Properties, deserialize_properties};

/// Value of the `format` field written by this library.
pub const FORMAT_NAME: &str = "graphydra";
/// Accepted on read for files produced from the original design document.
const LEGACY_FORMAT_NAMES: &[&str] = &["graphforge"];
/// Format version written by this library.
pub const FORMAT_VERSION: &str = "1.0";
const SUPPORTED_MAJOR: u64 = 1;
const SUPPORTED_MINOR: u64 = 0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotIn {
    format: String,
    format_version: String,
    graph_id: String,
    #[serde(default)]
    generation: u64,
    #[serde(default, deserialize_with = "deserialize_properties")]
    metadata: Properties,
    #[serde(default)]
    indexes: Vec<IndexDef>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    edges: Vec<Edge>,
}

/// Lenient header used only to produce a better error message.
#[derive(Deserialize)]
struct HeaderPeek {
    format: Option<String>,
    format_version: Option<String>,
}

/// A parsed, validated snapshot.
pub(crate) struct Snapshot {
    pub(crate) graph_id: String,
    pub(crate) generation: u64,
    pub(crate) store: Store,
}

fn check_header(format: &str, version: &str) -> Result<()> {
    if format != FORMAT_NAME && !LEGACY_FORMAT_NAMES.contains(&format) {
        return Err(GraphError::Format(format!("\"format\" is {format:?}; expected {FORMAT_NAME:?}")));
    }
    let unsupported = || GraphError::UnsupportedVersion {
        found: version.to_owned(),
        supported: format!("{SUPPORTED_MAJOR}.0 - {SUPPORTED_MAJOR}.{SUPPORTED_MINOR}"),
    };
    let (major, minor) = version.split_once('.').ok_or_else(unsupported)?;
    let major: u64 = major.parse().map_err(|_| unsupported())?;
    let minor: u64 = minor.parse().map_err(|_| unsupported())?;
    if major != SUPPORTED_MAJOR || minor > SUPPORTED_MINOR {
        return Err(unsupported());
    }
    Ok(())
}

/// Parse and validate snapshot bytes.
pub(crate) fn parse(bytes: &[u8], limits: &Limits) -> Result<Snapshot> {
    let raw: SnapshotIn = match serde_json::from_slice(bytes) {
        Ok(raw) => raw,
        Err(err) => {
            // A structural error may really be a version mismatch: report that instead.
            if let Ok(HeaderPeek { format: Some(f), format_version: Some(v) }) = serde_json::from_slice(bytes) {
                check_header(&f, &v)?;
            }
            return Err(GraphError::Format(format!("{err} (line {}, column {})", err.line(), err.column())));
        }
    };
    check_header(&raw.format, &raw.format_version)?;
    validate_id("graph", &raw.graph_id, limits)?;

    let mut store = Store::new(limits.clone());
    for (key, value) in &raw.metadata {
        validate_name("metadata key", key, limits)?;
        validate_value(key, value, limits)?;
    }
    store.metadata = raw.metadata;
    for node in raw.nodes {
        store.apply(&Op::AddNode { node })?;
    }
    for edge in raw.edges {
        store.apply(&Op::AddEdge { edge })?;
    }
    for index in raw.indexes {
        store.apply(&Op::CreateIndex { index })?;
    }
    Ok(Snapshot { graph_id: raw.graph_id, generation: raw.generation, store })
}

#[derive(Serialize)]
struct NodeOut<'a> {
    id: &'a str,
    labels: Vec<&'a str>,
    properties: &'a Properties,
}

#[derive(Serialize)]
struct EdgeOut<'a> {
    id: &'a str,
    source: &'a str,
    target: &'a str,
    #[serde(rename = "type")]
    edge_type: &'a str,
    #[serde(skip_serializing_if = "is_true")]
    directed: bool,
    properties: &'a Properties,
}

/// `Write` adapter that hashes everything written through it.
pub(crate) struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        HashingWriter { inner, hasher: Sha256::new() }
    }

    pub(crate) fn finish(self) -> (W, [u8; 32]) {
        let digest = self.hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(digest.as_slice());
        (self.inner, out)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_slice());
    out
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_true(b: &bool) -> bool {
    *b
}

fn json<T: Serialize + ?Sized>(v: &T) -> io::Result<String> {
    serde_json::to_string(v).map_err(json_err)
}

fn json_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Serialize a store as a canonical snapshot. Records are written one per
/// line in slot order, which keeps files diff-friendly and deterministic.
pub(crate) fn write<W: Write>(out: &mut W, graph_id: &str, generation: u64, store: &Store) -> io::Result<()> {
    writeln!(out, "{{")?;
    writeln!(out, "  \"format\": {},", json(&FORMAT_NAME)?)?;
    writeln!(out, "  \"format_version\": {},", json(&FORMAT_VERSION)?)?;
    writeln!(out, "  \"graph_id\": {},", json(&graph_id)?)?;
    writeln!(out, "  \"generation\": {generation},")?;
    writeln!(out, "  \"metadata\": {},", json(&store.metadata)?)?;
    writeln!(out, "  \"indexes\": {},", json(&store.index_defs())?)?;

    writeln!(out, "  \"nodes\": [")?;
    let mut first = true;
    for slot in store.node_slots_iter() {
        let n = store.node(slot).expect("live slot");
        let rec = NodeOut {
            id: &n.id,
            labels: n.labels.iter().map(|l| store.labels.name(*l)).collect(),
            properties: &n.props,
        };
        if !first {
            writeln!(out, ",")?;
        }
        first = false;
        write!(out, "    ")?;
        serde_json::to_writer(&mut *out, &rec).map_err(json_err)?;
    }
    if !first {
        writeln!(out)?;
    }
    writeln!(out, "  ],")?;

    writeln!(out, "  \"edges\": [")?;
    let mut first = true;
    for slot in store.edge_slots_iter() {
        let e = store.edge(slot).expect("live slot");
        let rec = EdgeOut {
            id: &e.id,
            source: &store.node(e.src).expect("endpoint").id,
            target: &store.node(e.tgt).expect("endpoint").id,
            edge_type: store.types.name(e.ty),
            directed: e.directed,
            properties: &e.props,
        };
        if !first {
            writeln!(out, ",")?;
        }
        first = false;
        write!(out, "    ")?;
        serde_json::to_writer(&mut *out, &rec).map_err(json_err)?;
    }
    if !first {
        writeln!(out)?;
    }
    writeln!(out, "  ]")?;
    writeln!(out, "}}")?;
    Ok(())
}
