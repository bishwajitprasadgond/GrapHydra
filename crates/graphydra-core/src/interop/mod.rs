//! Import and export: JSON, JSON Lines, CSV and GraphML (export).
//!
//! Imports are atomic: either every record is added in one transaction or
//! nothing is. Input is parsed strictly (duplicate keys, invalid numbers and
//! unknown fields are rejected) and never interpreted as code or paths.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Write};

use serde::Serialize;

use crate::engine::mutate::generate_id;
use crate::error::{GraphError, Result};
use crate::graph::Graph;
use crate::model::{Edge, Node};
use crate::ops::Op;
use crate::value::{Properties, Value};

/// Counts of imported records.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub nodes: usize,
    pub edges: usize,
}

fn fmt_err(msg: impl Into<String>) -> GraphError {
    GraphError::Format(msg.into())
}

fn read_all(mut r: impl Read, max: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.by_ref().take(max + 1).read_to_end(&mut buf).map_err(|e| GraphError::io("reading import data", e))?;
    if buf.len() as u64 > max {
        return Err(GraphError::LimitExceeded(format!("import data exceeds {max} bytes")));
    }
    Ok(buf)
}

fn take_map(v: Value, what: &str) -> Result<BTreeMap<String, Value>> {
    match v {
        Value::Map(m) => Ok(m),
        other => Err(fmt_err(format!("{what} must be a JSON object, got {}", other.type_name()))),
    }
}

fn take_string(m: &mut BTreeMap<String, Value>, key: &str, what: &str) -> Result<Option<String>> {
    match m.remove(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(Value::Int(i)) if key == "id" || key == "source" || key == "target" => Ok(Some(i.to_string())),
        Some(other) => Err(fmt_err(format!("{what}: \"{key}\" must be a string, got {}", other.type_name()))),
    }
}

fn take_props(m: &mut BTreeMap<String, Value>, what: &str) -> Result<Properties> {
    match m.remove("properties") {
        None | Some(Value::Null) => Ok(Properties::new()),
        Some(Value::Map(p)) => Ok(p),
        Some(other) => Err(fmt_err(format!("{what}: \"properties\" must be an object, got {}", other.type_name()))),
    }
}

fn reject_unknown(m: &BTreeMap<String, Value>, what: &str) -> Result<()> {
    match m.keys().next() {
        Some(k) => Err(fmt_err(format!("{what}: unknown field {k:?}"))),
        None => Ok(()),
    }
}

fn node_from_json(v: Value, what: &str) -> Result<Node> {
    let mut m = take_map(v, what)?;
    m.remove("kind");
    let id = take_string(&mut m, "id", what)?.ok_or_else(|| fmt_err(format!("{what}: missing \"id\"")))?;
    let labels = match m.remove("labels") {
        None | Some(Value::Null) => BTreeSet::new(),
        Some(Value::List(items)) => {
            let mut set = BTreeSet::new();
            for i in items {
                match i {
                    Value::String(s) => {
                        set.insert(s);
                    }
                    other => return Err(fmt_err(format!("{what}: labels must be strings, got {}", other.type_name()))),
                }
            }
            set
        }
        Some(Value::String(s)) => BTreeSet::from([s]),
        Some(other) => return Err(fmt_err(format!("{what}: \"labels\" must be a list, got {}", other.type_name()))),
    };
    let properties = take_props(&mut m, what)?;
    reject_unknown(&m, what)?;
    Ok(Node { id, labels, properties })
}

fn edge_from_json(v: Value, what: &str) -> Result<(Option<String>, Edge)> {
    let mut m = take_map(v, what)?;
    m.remove("kind");
    let id = take_string(&mut m, "id", what)?;
    let source = take_string(&mut m, "source", what)?.ok_or_else(|| fmt_err(format!("{what}: missing \"source\"")))?;
    let target = take_string(&mut m, "target", what)?.ok_or_else(|| fmt_err(format!("{what}: missing \"target\"")))?;
    let edge_type = take_string(&mut m, "type", what)?.ok_or_else(|| fmt_err(format!("{what}: missing \"type\"")))?;
    let directed = match m.remove("directed") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => b,
        Some(other) => {
            return Err(fmt_err(format!("{what}: \"directed\" must be a boolean, got {}", other.type_name())));
        }
    };
    let properties = take_props(&mut m, what)?;
    reject_unknown(&m, what)?;
    let edge = Edge { id: id.clone().unwrap_or_default(), source, target, edge_type, directed, properties };
    Ok((id, edge))
}

/// Add nodes then edges in one transaction, generating missing edge ids.
fn import_records(graph: &mut Graph, nodes: Vec<Node>, edges: Vec<(Option<String>, Edge)>) -> Result<ImportReport> {
    let report = ImportReport { nodes: nodes.len(), edges: edges.len() };
    let mut ops: Vec<Op> = nodes.into_iter().map(|node| Op::AddNode { node }).collect();
    let mut reserved: BTreeSet<String> = edges.iter().filter_map(|(id, _)| id.clone()).collect();
    for (id, mut edge) in edges {
        if id.is_none() {
            let mut fresh = generate_id(graph.store_ref(), "e", false);
            while reserved.contains(&fresh) {
                fresh = generate_id(graph.store_ref(), "e", false);
            }
            reserved.insert(fresh.clone());
            edge.id = fresh;
        }
        ops.push(Op::AddEdge { edge });
    }
    graph.apply_ops(ops)?;
    Ok(report)
}

/// Import a JSON document: either a full `.graph` document or
/// `{"nodes": [...], "edges": [...]}`.
pub fn import_json(graph: &mut Graph, reader: impl Read) -> Result<ImportReport> {
    let bytes = read_all(reader, graph.config().limits.max_file_bytes)?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| fmt_err(format!("invalid JSON: {e}")))?;
    let mut doc = take_map(v, "document")?;
    for k in ["format", "format_version", "graph_id", "generation", "metadata"] {
        doc.remove(k);
    }
    let indexes = doc.remove("indexes");
    let nodes = match doc.remove("nodes") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::List(items)) => items
            .into_iter()
            .enumerate()
            .map(|(i, n)| node_from_json(n, &format!("nodes[{i}]")))
            .collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(fmt_err("\"nodes\" must be a list")),
    };
    let edges = match doc.remove("edges") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::List(items)) => items
            .into_iter()
            .enumerate()
            .map(|(i, e)| edge_from_json(e, &format!("edges[{i}]")))
            .collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(fmt_err("\"edges\" must be a list")),
    };
    reject_unknown(&doc, "document")?;
    let _ = indexes; // index definitions are local configuration and are not imported
    import_records(graph, nodes, edges)
}

/// Export as a canonical `.graph` JSON document.
pub fn export_json(graph: &Graph, out: &mut dyn Write) -> Result<()> {
    graph.write_snapshot_to(out)
}

/// Import JSON Lines: one object per line with `"kind": "node"` or `"edge"`.
pub fn import_jsonl(graph: &mut Graph, reader: impl BufRead) -> Result<ImportReport> {
    let max = graph.config().limits.max_file_bytes;
    let mut total = 0u64;
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| GraphError::io("reading JSON Lines", e))?;
        total += line.len() as u64 + 1;
        if total > max {
            return Err(GraphError::LimitExceeded(format!("import data exceeds {max} bytes")));
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let what = format!("line {}", i + 1);
        let v: Value = serde_json::from_str(trimmed).map_err(|e| fmt_err(format!("{what}: invalid JSON: {e}")))?;
        let kind = match &v {
            Value::Map(m) => m.get("kind").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        };
        match kind.as_deref() {
            Some("node") => nodes.push(node_from_json(v, &what)?),
            Some("edge") => edges.push(edge_from_json(v, &what)?),
            _ => return Err(fmt_err(format!("{what}: expected \"kind\": \"node\" or \"edge\""))),
        }
    }
    import_records(graph, nodes, edges)
}

#[derive(Serialize)]
struct NodeLine<'a> {
    kind: &'static str,
    #[serde(flatten)]
    node: &'a Node,
}

#[derive(Serialize)]
struct EdgeLine<'a> {
    kind: &'static str,
    #[serde(flatten)]
    edge: &'a Edge,
}

pub fn export_jsonl(graph: &Graph, out: &mut dyn Write) -> Result<()> {
    let io = |e: std::io::Error| GraphError::io("writing JSON Lines", e);
    let mut w = std::io::BufWriter::new(out);
    for n in graph.nodes() {
        serde_json::to_writer(&mut w, &NodeLine { kind: "node", node: &n }).map_err(|e| io(e.into()))?;
        w.write_all(b"\n").map_err(io)?;
    }
    for e in graph.edges() {
        serde_json::to_writer(&mut w, &EdgeLine { kind: "edge", edge: &e }).map_err(|e| io(e.into()))?;
        w.write_all(b"\n").map_err(io)?;
    }
    w.flush().map_err(io)
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColType {
    String,
    Int,
    Float,
    Bool,
    Json,
}

impl ColType {
    fn parse(s: &str) -> Option<ColType> {
        Some(match s.to_ascii_lowercase().as_str() {
            "string" | "str" => ColType::String,
            "int" | "integer" | "long" => ColType::Int,
            "float" | "double" | "number" => ColType::Float,
            "bool" | "boolean" => ColType::Bool,
            "json" => ColType::Json,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            ColType::String => "string",
            ColType::Int => "int",
            ColType::Float => "float",
            ColType::Bool => "bool",
            ColType::Json => "json",
        }
    }

    fn convert(self, cell: &str, what: &str) -> Result<Value> {
        let bad = |t: &str| fmt_err(format!("{what}: {cell:?} is not a valid {t}"));
        Ok(match self {
            ColType::String => Value::String(cell.to_owned()),
            ColType::Int => Value::Int(cell.trim().parse().map_err(|_| bad("integer"))?),
            ColType::Float => {
                let f: f64 = cell.trim().parse().map_err(|_| bad("float"))?;
                if !f.is_finite() {
                    return Err(bad("finite float"));
                }
                Value::Float(f)
            }
            ColType::Bool => match cell.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" => Value::Bool(true),
                "false" | "0" | "no" => Value::Bool(false),
                _ => return Err(bad("boolean")),
            },
            ColType::Json => serde_json::from_str(cell).map_err(|e| fmt_err(format!("{what}: invalid JSON: {e}")))?,
        })
    }
}

struct Column {
    name: String,
    ty: ColType,
}

fn parse_header(headers: &csv::StringRecord) -> Result<Vec<Column>> {
    headers
        .iter()
        .map(|h| {
            let (name, ty) = match h.rsplit_once(':') {
                Some((n, t)) => match ColType::parse(t) {
                    Some(ty) => (n, ty),
                    None => return Err(fmt_err(format!("CSV header {h:?}: unknown type {t:?}"))),
                },
                None => (h, ColType::String),
            };
            Ok(Column { name: name.trim().to_owned(), ty })
        })
        .collect()
}

fn csv_reader(r: impl Read) -> csv::Reader<impl Read> {
    csv::ReaderBuilder::new().has_headers(true).flexible(false).from_reader(r)
}

fn csv_err(e: csv::Error) -> GraphError {
    fmt_err(format!("CSV: {e}"))
}

/// Import nodes and/or edges from CSV.
///
/// Node columns: `id`, optional `labels` (`;`-separated), then properties.
/// Edge columns: optional `id`, `source`, `target`, `type`, optional
/// `directed`, then properties. A header may carry a type suffix
/// (`risk:float`, `tags:json`); empty cells are omitted.
pub fn import_csv(graph: &mut Graph, nodes: Option<impl Read>, edges: Option<impl Read>) -> Result<ImportReport> {
    let mut node_list = Vec::new();
    if let Some(r) = nodes {
        let mut rdr = csv_reader(r);
        let cols = parse_header(rdr.headers().map_err(csv_err)?)?;
        if !cols.iter().any(|c| c.name == "id") {
            return Err(fmt_err("nodes CSV needs an \"id\" column"));
        }
        for (i, rec) in rdr.records().enumerate() {
            let rec = rec.map_err(csv_err)?;
            let what = format!("nodes CSV row {}", i + 2);
            let mut node = Node::new("");
            for (col, cell) in cols.iter().zip(rec.iter()) {
                match col.name.as_str() {
                    "id" => node.id = cell.to_owned(),
                    "labels" => {
                        node.labels =
                            cell.split(';').map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect()
                    }
                    _ if cell.is_empty() => {}
                    name => {
                        node.properties
                            .insert(name.to_owned(), col.ty.convert(cell, &format!("{what}, column {name}"))?);
                    }
                }
            }
            node_list.push(node);
        }
    }
    let mut edge_list = Vec::new();
    if let Some(r) = edges {
        let mut rdr = csv_reader(r);
        let cols = parse_header(rdr.headers().map_err(csv_err)?)?;
        for required in ["source", "target", "type"] {
            if !cols.iter().any(|c| c.name == required) {
                return Err(fmt_err(format!("edges CSV needs a {required:?} column")));
            }
        }
        for (i, rec) in rdr.records().enumerate() {
            let rec = rec.map_err(csv_err)?;
            let what = format!("edges CSV row {}", i + 2);
            let mut id = None;
            let mut edge = Edge::new("", "", "", "");
            for (col, cell) in cols.iter().zip(rec.iter()) {
                match col.name.as_str() {
                    "id" => id = (!cell.is_empty()).then(|| cell.to_owned()),
                    "source" => edge.source = cell.to_owned(),
                    "target" => edge.target = cell.to_owned(),
                    "type" => edge.edge_type = cell.to_owned(),
                    "directed" if !cell.is_empty() => {
                        edge.directed = matches!(ColType::Bool.convert(cell, &what)?, Value::Bool(true));
                    }
                    _ if cell.is_empty() => {}
                    name => {
                        edge.properties
                            .insert(name.to_owned(), col.ty.convert(cell, &format!("{what}, column {name}"))?);
                    }
                }
            }
            edge.id = id.clone().unwrap_or_default();
            edge_list.push((id, edge));
        }
    }
    import_records(graph, node_list, edge_list)
}

fn infer_type<'a>(values: impl Iterator<Item = &'a Value>) -> ColType {
    let mut ty: Option<ColType> = None;
    for v in values {
        let t = match v {
            Value::String(_) => ColType::String,
            Value::Int(_) => ColType::Int,
            Value::Float(_) => ColType::Float,
            Value::Bool(_) => ColType::Bool,
            _ => return ColType::Json,
        };
        ty = match (ty, t) {
            (None, t) => Some(t),
            (Some(ColType::Int), ColType::Float) | (Some(ColType::Float), ColType::Int) => Some(ColType::Float),
            (Some(a), b) if a == b => Some(a),
            _ => return ColType::Json,
        };
    }
    ty.unwrap_or(ColType::String)
}

fn cell(v: Option<&Value>, ty: ColType) -> Result<String> {
    Ok(match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) if ty == ColType::String => s.clone(),
        Some(Value::Int(i)) if ty != ColType::Json => i.to_string(),
        Some(Value::Float(f)) if ty != ColType::Json => {
            let s = f.to_string();
            if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
        }
        Some(Value::Bool(b)) if ty != ColType::Json => b.to_string(),
        Some(other) => serde_json::to_string(other).map_err(|e| fmt_err(e.to_string()))?,
    })
}

/// Export nodes and edges to two CSV streams with typed headers.
pub fn export_csv(graph: &Graph, nodes_out: &mut dyn Write, edges_out: &mut dyn Write) -> Result<()> {
    let nodes: Vec<Node> = graph.nodes().collect();
    let keys: BTreeSet<&String> = nodes.iter().flat_map(|n| n.properties.keys()).collect();
    let types: Vec<(&String, ColType)> =
        keys.iter().map(|k| (*k, infer_type(nodes.iter().filter_map(|n| n.properties.get(*k))))).collect();
    let mut w = csv::Writer::from_writer(nodes_out);
    let mut header = vec!["id".to_owned(), "labels".to_owned()];
    header.extend(types.iter().map(|(k, t)| format!("{k}:{}", t.name())));
    w.write_record(&header).map_err(csv_err)?;
    for n in &nodes {
        let mut rec = vec![n.id.clone(), n.labels.iter().cloned().collect::<Vec<_>>().join(";")];
        for (k, t) in &types {
            rec.push(cell(n.properties.get(*k), *t)?);
        }
        w.write_record(&rec).map_err(csv_err)?;
    }
    w.flush().map_err(|e| GraphError::io("writing CSV", e))?;

    let edges: Vec<Edge> = graph.edges().collect();
    let keys: BTreeSet<&String> = edges.iter().flat_map(|e| e.properties.keys()).collect();
    let types: Vec<(&String, ColType)> =
        keys.iter().map(|k| (*k, infer_type(edges.iter().filter_map(|e| e.properties.get(*k))))).collect();
    let mut w = csv::Writer::from_writer(edges_out);
    let mut header: Vec<String> = ["id", "source", "target", "type", "directed:bool"].map(String::from).to_vec();
    header.extend(types.iter().map(|(k, t)| format!("{k}:{}", t.name())));
    w.write_record(&header).map_err(csv_err)?;
    for e in &edges {
        let mut rec =
            vec![e.id.clone(), e.source.clone(), e.target.clone(), e.edge_type.clone(), e.directed.to_string()];
        for (k, t) in &types {
            rec.push(cell(e.properties.get(*k), *t)?);
        }
        w.write_record(&rec).map_err(csv_err)?;
    }
    w.flush().map_err(|e| GraphError::io("writing CSV", e))
}

// ---------------------------------------------------------------------------
// GraphML (export only)
// ---------------------------------------------------------------------------

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => out.push_str(&format!("&#x{:x};", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn graphml_type(t: ColType) -> &'static str {
    match t {
        ColType::Int => "long",
        ColType::Float => "double",
        ColType::Bool => "boolean",
        _ => "string",
    }
}

/// Export as GraphML. Labels are written to the `labels` key (`;`-joined),
/// edge types to `type`; nested values are JSON-encoded strings.
pub fn export_graphml(graph: &Graph, out: &mut dyn Write) -> Result<()> {
    let io = |e: std::io::Error| GraphError::io("writing GraphML", e);
    let mut w = std::io::BufWriter::new(out);
    let nodes: Vec<Node> = graph.nodes().collect();
    let edges: Vec<Edge> = graph.edges().collect();
    let node_keys: BTreeSet<&String> = nodes.iter().flat_map(|n| n.properties.keys()).collect();
    let edge_keys: BTreeSet<&String> = edges.iter().flat_map(|e| e.properties.keys()).collect();
    let node_types: Vec<(&String, ColType)> =
        node_keys.iter().map(|k| (*k, infer_type(nodes.iter().filter_map(|n| n.properties.get(*k))))).collect();
    let edge_types: Vec<(&String, ColType)> =
        edge_keys.iter().map(|k| (*k, infer_type(edges.iter().filter_map(|e| e.properties.get(*k))))).collect();

    writeln!(w, r#"<?xml version="1.0" encoding="UTF-8"?>"#).map_err(io)?;
    writeln!(w, r#"<graphml xmlns="http://graphml.graphdrawing.org/xmlns">"#).map_err(io)?;
    writeln!(w, r#"  <key id="labels" for="node" attr.name="labels" attr.type="string"/>"#).map_err(io)?;
    writeln!(w, r#"  <key id="type" for="edge" attr.name="type" attr.type="string"/>"#).map_err(io)?;
    for (i, (k, t)) in node_types.iter().enumerate() {
        writeln!(w, r#"  <key id="n{i}" for="node" attr.name="{}" attr.type="{}"/>"#, xml_escape(k), graphml_type(*t))
            .map_err(io)?;
    }
    for (i, (k, t)) in edge_types.iter().enumerate() {
        writeln!(w, r#"  <key id="e{i}" for="edge" attr.name="{}" attr.type="{}"/>"#, xml_escape(k), graphml_type(*t))
            .map_err(io)?;
    }
    writeln!(w, r#"  <graph id="{}" edgedefault="directed">"#, xml_escape(graph.graph_id())).map_err(io)?;
    for n in &nodes {
        writeln!(w, r#"    <node id="{}">"#, xml_escape(&n.id)).map_err(io)?;
        let labels = n.labels.iter().cloned().collect::<Vec<_>>().join(";");
        writeln!(w, r#"      <data key="labels">{}</data>"#, xml_escape(&labels)).map_err(io)?;
        for (i, (k, t)) in node_types.iter().enumerate() {
            if let Some(v) = n.properties.get(*k) {
                writeln!(w, r#"      <data key="n{i}">{}</data>"#, xml_escape(&cell(Some(v), *t)?)).map_err(io)?;
            }
        }
        writeln!(w, "    </node>").map_err(io)?;
    }
    for e in &edges {
        writeln!(
            w,
            r#"    <edge id="{}" source="{}" target="{}" directed="{}">"#,
            xml_escape(&e.id),
            xml_escape(&e.source),
            xml_escape(&e.target),
            e.directed
        )
        .map_err(io)?;
        writeln!(w, r#"      <data key="type">{}</data>"#, xml_escape(&e.edge_type)).map_err(io)?;
        for (i, (k, t)) in edge_types.iter().enumerate() {
            if let Some(v) = e.properties.get(*k) {
                writeln!(w, r#"      <data key="e{i}">{}</data>"#, xml_escape(&cell(Some(v), *t)?)).map_err(io)?;
            }
        }
        writeln!(w, "    </edge>").map_err(io)?;
    }
    writeln!(w, "  </graph>\n</graphml>").map_err(io)?;
    w.flush().map_err(io)
}
