//! Persistence, recovery, locking and file-format tests (spec sections 4, 8, 12).

use std::fs;
use std::path::{Path, PathBuf};

use graphydra_core::{
    Config, Durability, Edge, Graph, GraphError, IndexDef, IndexKind, IndexTarget, Node, OpenOptions, Value,
    validate_file,
};

fn journal(p: &Path) -> PathBuf {
    PathBuf::from(format!("{}.journal", p.display()))
}

fn nosync() -> OpenOptions {
    OpenOptions::new().config(Config { durability: Durability::NoSync, ..Config::default() })
}

fn sample(g: &mut Graph) {
    g.add_node(Node::new("a").with_label("A").with_property("x", 1)).expect("a");
    g.add_node(Node::new("b").with_label("B").with_property("s", "héllo \"quoted\" \n line")).expect("b");
    g.add_edge(Edge::new("ab", "a", "b", "REL").with_property("w", 0.5)).expect("ab");
    g.add_edge(Edge::new("ba", "b", "a", "PEER").undirected()).expect("ba");
    g.create_index(IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "x")).expect("idx");
    g.set_metadata("description", "test graph").expect("meta");
}

fn logical(g: &Graph) -> (Vec<Node>, Vec<Edge>, Vec<IndexDef>, Vec<(String, Value)>) {
    (
        g.nodes().collect(),
        g.edges().collect(),
        g.indexes().into_iter().map(|i| i.def).collect(),
        g.metadata().clone().into_iter().collect(),
    )
}

#[test]
fn create_save_close_reopen_preserves_everything() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let mut g = Graph::create(&path).expect("create");
    sample(&mut g);
    let before = logical(&g);
    let generation = g.generation();
    g.close().expect("close");

    let text = fs::read_to_string(&path).expect("read");
    assert!(text.contains("\"format\": \"graphydra\""));
    assert!(text.contains("\"directed\":false"));

    let g = Graph::open(&path).expect("reopen");
    assert_eq!(logical(&g), before);
    assert_eq!(g.generation(), generation);
    assert_eq!(g.graph_id(), "t");
    assert!(g.verify().is_empty());
    assert!(Graph::create(&path).is_err(), "create must not overwrite");
}

#[test]
fn commits_survive_without_checkpoint() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let before;
    {
        let mut g = Graph::create(&path).expect("create");
        sample(&mut g);
        before = logical(&g);
        // Simulate a crash: drop without close() -> snapshot is stale, journal has the data.
    }
    let snapshot = fs::read_to_string(&path).expect("read");
    assert!(!snapshot.contains("\"a\""), "data should only be in the journal");
    let g = Graph::open(&path).expect("reopen");
    assert_eq!(logical(&g), before);
    assert_eq!(g.stats().recovered_transactions, 6);
}

#[test]
fn uncommitted_transaction_is_discarded() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    {
        let mut g = Graph::create(&path).expect("create");
        g.add_node(Node::new("kept")).expect("kept");
        g.begin().expect("begin");
        g.add_node(Node::new("lost")).expect("lost");
        // process "crashes" with the transaction open
    }
    let g = Graph::open(&path).expect("reopen");
    assert!(g.contains_node("kept"));
    assert!(!g.contains_node("lost"));
}

#[test]
fn torn_journal_writes_recover_at_every_byte() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    {
        let mut g = Graph::create(&path).expect("create");
        g.add_node(Node::new("n1")).expect("n1");
        g.add_node(Node::new("n2")).expect("n2");
        g.add_node(Node::new("n3")).expect("n3");
    }
    let full = fs::read(journal(&path)).expect("journal");
    let snapshot = fs::read(&path).expect("snapshot");
    let line_ends: Vec<usize> = full.iter().enumerate().filter(|(_, b)| **b == b'\n').map(|(i, _)| i + 1).collect();
    for cut in 0..=full.len() {
        let case = dir.path().join(format!("c{cut}.graph"));
        // Keep the original graph_id so the journal header matches.
        fs::write(&case, &snapshot).expect("copy");
        fs::write(journal(&case), &full[..cut]).expect("cut");
        let g = Graph::open_with(&case, nosync()).unwrap_or_else(|e| panic!("cut {cut}: {e}"));
        // Number of complete records (lines after the header).
        let complete = line_ends.iter().filter(|e| **e <= cut).count().saturating_sub(1);
        assert_eq!(g.node_count(), complete, "cut at byte {cut}");
        assert!(g.verify().is_empty());
        drop(g);
        // A second open sees the same (repaired) state.
        assert_eq!(Graph::open_with(&case, nosync()).expect("reopen").node_count(), complete);
    }
}

#[test]
fn crash_between_snapshot_rename_and_journal_reset() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let mut g = Graph::create(&path).expect("create");
    g.add_node(Node::new("a")).expect("a");
    g.add_node(Node::new("b")).expect("b");
    let stale_journal = fs::read(journal(&path)).expect("journal");
    g.checkpoint().expect("checkpoint");
    g.add_node(Node::new("c")).expect("c");
    drop(g);
    // Put back the pre-checkpoint journal records ahead of the new one:
    // records already in the snapshot must be skipped, not re-applied.
    let current = fs::read(journal(&path)).expect("journal");
    let header_end = current.iter().position(|b| *b == b'\n').expect("header") + 1;
    let mut merged = stale_journal.clone();
    merged.extend_from_slice(&current[header_end..]);
    fs::write(journal(&path), merged).expect("write");
    let g = Graph::open(&path).expect("reopen");
    assert_eq!(g.node_count(), 3);
}

#[test]
fn second_writer_is_rejected_readers_are_not() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let mut writer = Graph::create(&path).expect("create");
    writer.add_node(Node::new("a")).expect("a");
    assert!(matches!(Graph::open(&path), Err(GraphError::Locked { .. })));
    let mut reader = Graph::open_read_only(&path).expect("reader");
    assert!(reader.contains_node("a"));
    assert!(matches!(reader.add_node(Node::new("x")), Err(GraphError::ReadOnly)));
    assert!(matches!(reader.run("CREATE (:X)"), Err(GraphError::ReadOnly)));
    writer.add_node(Node::new("b")).expect("b");
    assert!(!reader.contains_node("b"), "readers see a snapshot");
    reader.refresh().expect("refresh");
    assert!(reader.contains_node("b"));
    writer.close().expect("close");
    let again = Graph::open(&path).expect("lock released after close");
    assert_eq!(again.node_count(), 2);
}

#[test]
fn external_modification_is_detected() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let mut g = Graph::create(&path).expect("create");
    g.add_node(Node::new("a")).expect("a");
    let text = fs::read_to_string(&path).expect("read");
    fs::write(&path, text.replace("\"generation\": 0", "\"generation\": 0 ")).expect("tamper");
    assert!(matches!(g.checkpoint(), Err(GraphError::ExternalModification(_))));
    // The committed node is still safe in the journal.
    drop(g);
    assert!(Graph::open(&path).expect("reopen").contains_node("a"));
}

fn open_bytes(dir: &Path, name: &str, body: &str) -> Result<Graph, GraphError> {
    let p = dir.join(name);
    fs::write(&p, body).expect("write");
    Graph::open(&p)
}

#[test]
fn rejects_malformed_and_malicious_files() {
    let dir = tempfile::tempdir().expect("tmp");
    let d = dir.path();
    let head = r#""format":"graphydra","format_version":"1.0","graph_id":"g""#;
    let cases = [
        ("dup_key", format!(r#"{{{head},"nodes":[{{"id":"a","properties":{{"x":1,"x":2}}}}]}}"#)),
        ("dup_field", format!(r#"{{{head},"graph_id":"h"}}"#)),
        ("dup_node", format!(r#"{{{head},"nodes":[{{"id":"a"}},{{"id":"a"}}]}}"#)),
        (
            "dangling",
            format!(
                r#"{{{head},"nodes":[{{"id":"a"}}],"edges":[{{"id":"e","source":"a","target":"zz","type":"T"}}]}}"#
            ),
        ),
        ("unknown_field", format!(r#"{{{head},"script":"rm -rf /"}}"#)),
        ("bad_number", format!(r#"{{{head},"nodes":[{{"id":"a","properties":{{"x":1e999}}}}]}}"#)),
        ("big_int", format!(r#"{{{head},"nodes":[{{"id":"a","properties":{{"x":18446744073709551615}}}}]}}"#)),
        ("reserved_key", format!(r#"{{{head},"nodes":[{{"id":"a","properties":{{"id":"b"}}}}]}}"#)),
        ("dup_label", format!(r#"{{{head},"nodes":[{{"id":"a","labels":["X","X"]}}]}}"#)),
        ("empty_id", format!(r#"{{{head},"nodes":[{{"id":""}}]}}"#)),
        (
            "deep",
            format!(r#"{{{head},"nodes":[{{"id":"a","properties":{{"x":{}1{}}}}}]}}"#, "[".repeat(40), "]".repeat(40)),
        ),
        ("not_json", "this is not json".to_owned()),
        ("wrong_format", r#"{"format":"other","format_version":"1.0","graph_id":"g"}"#.to_owned()),
    ];
    for (name, body) in &cases {
        let r = open_bytes(d, &format!("{name}.graph"), body);
        assert!(r.is_err(), "{name} should be rejected");
    }
    let v2 = r#"{"format":"graphydra","format_version":"2.0","graph_id":"g","new_thing":[]}"#;
    assert!(matches!(open_bytes(d, "v2.graph", v2), Err(GraphError::UnsupportedVersion { .. })));
    let v11 = r#"{"format":"graphydra","format_version":"1.1","graph_id":"g"}"#;
    assert!(matches!(open_bytes(d, "v11.graph", v11), Err(GraphError::UnsupportedVersion { .. })));
}

#[test]
fn reads_the_design_document_example() {
    let dir = tempfile::tempdir().expect("tmp");
    let body = r#"
{
  "format": "graphforge",
  "format_version": "1.0",
  "graph_id": "cybersecurity-demo",
  "metadata": {"description": "Example cybersecurity knowledge graph", "created_by": "GraphForge"},
  "nodes": [
    {"id": "host-001", "labels": ["Host"], "properties": {"hostname": "workstation-01", "risk_score": 0.92}},
    {"id": "malware-001", "labels": ["Malware"], "properties": {"family": "ExampleFamily", "severity": "high"}},
    {"id": "ioc-001", "labels": ["Indicator"], "properties": {"indicator_type": "domain", "value": "example.invalid"}}
  ],
  "edges": [
    {"id": "edge-001", "source": "host-001", "target": "malware-001", "type": "DETECTED", "properties": {"confidence": 0.97}},
    {"id": "edge-002", "source": "malware-001", "target": "ioc-001", "type": "USES_INDICATOR", "properties": {}}
  ]
}"#;
    let mut g = open_bytes(dir.path(), "cybersecurity.graph", body).expect("spec example opens");
    assert_eq!(g.graph_id(), "cybersecurity-demo");
    let r = g.run("MATCH (h:Host)-[:DETECTED]->(m:Malware) WHERE h.id = 'host-001' RETURN m.family").expect("q");
    assert_eq!(r.rows.len(), 1);
    let report = validate_file(dir.path().join("cybersecurity.graph"));
    assert!(report.valid, "{report:?}");
    assert_eq!((report.nodes, report.edges), (3, 2));
}

#[test]
fn file_limits_are_enforced() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let mut g = Graph::create(&path).expect("create");
    g.add_nodes((0..100).map(|i| Node::new(format!("n{i}")))).expect("nodes");
    g.close().expect("close");
    let mut config = Config::default();
    config.limits.max_file_bytes = 200;
    let r = Graph::open_with(&path, OpenOptions::new().config(config));
    assert!(matches!(r, Err(GraphError::LimitExceeded(_))));
}

#[test]
fn auto_checkpoint_and_compaction() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    let config = Config { checkpoint_journal_bytes: 2_000, durability: Durability::NoSync, ..Config::default() };
    let mut g = Graph::open_with(&path, OpenOptions::new().create(true).config(config)).expect("create");
    for i in 0..200 {
        g.add_node(Node::new(format!("n{i}")).with_property("i", i)).expect("node");
    }
    assert!(g.stats().journal_bytes < 2_500, "journal should be checkpointed periodically");
    for i in 0..150 {
        g.delete_node(&format!("n{i}"), false).expect("delete");
    }
    g.checkpoint().expect("checkpoint");
    // Checkpoints compact once more than 25% of slots are tombstones.
    assert!(g.stats().fragmentation <= 0.25, "checkpoint compacts tombstones: {}", g.stats().fragmentation);
    let before = logical(&g);
    g.close().expect("close");
    assert_eq!(logical(&Graph::open(&path).expect("reopen")), before);
}

#[test]
fn validator_reports_pending_and_torn_journal() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("t.graph");
    {
        let mut g = Graph::create(&path).expect("create");
        g.add_node(Node::new("a")).expect("a");
    }
    let mut j = fs::read(journal(&path)).expect("journal");
    j.extend_from_slice(b"deadbeef {\"gen\":2,\"op");
    fs::write(journal(&path), j).expect("write");
    let report = validate_file(&path);
    assert!(report.valid, "{report:?}");
    assert_eq!(report.pending_journal_records, 1);
    assert_eq!(report.warnings.len(), 2);
    // Validation is read-only: the torn tail is still there.
    assert!(fs::read(journal(&path)).expect("journal").ends_with(b"\"op"));
}
