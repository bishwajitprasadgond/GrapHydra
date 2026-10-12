//! Query-language integration tests (spec sections 5 and 6).

use std::time::Duration;

use graphydra_core::{
    CancellationToken, Edge, Graph, GraphError, IndexDef, IndexKind, IndexTarget, Node, QueryOptions, QueryResult,
    ResultValue, Value,
};

fn cyber() -> Graph {
    let mut g = Graph::in_memory();
    g.add_nodes([
        Node::new("host-001")
            .with_label("Host")
            .with_property("hostname", "workstation-01")
            .with_property("risk_score", 0.92),
        Node::new("host-002")
            .with_label("Host")
            .with_property("hostname", "workstation-02")
            .with_property("risk_score", 0.35),
        Node::new("host-003")
            .with_label("Host")
            .with_property("hostname", "server-01")
            .with_property("risk_score", 0.81),
        Node::new("malware-001")
            .with_label("Malware")
            .with_property("family", "ExampleFamily")
            .with_property("severity", "high"),
        Node::new("malware-002")
            .with_label("Malware")
            .with_property("family", "OtherFamily")
            .with_property("severity", "low"),
        Node::new("ioc-001")
            .with_label("Indicator")
            .with_property("indicator_type", "domain")
            .with_property("value", "example.invalid"),
        Node::new("ioc-002")
            .with_label("Indicator")
            .with_property("indicator_type", "ip")
            .with_property("value", "192.0.2.1"),
    ])
    .expect("nodes");
    g.add_edges([
        Edge::new("edge-001", "host-001", "malware-001", "DETECTED").with_property("confidence", 0.97),
        Edge::new("edge-002", "malware-001", "ioc-001", "USES_INDICATOR"),
        Edge::new("edge-003", "host-003", "malware-001", "DETECTED").with_property("confidence", 0.5),
        Edge::new("edge-004", "host-003", "malware-002", "DETECTED").with_property("confidence", 0.7),
        Edge::new("edge-005", "malware-002", "ioc-002", "USES_INDICATOR"),
    ])
    .expect("edges");
    g
}

fn run(g: &mut Graph, q: &str) -> QueryResult {
    g.run(q).unwrap_or_else(|e| panic!("{q}\n{e}"))
}

fn strings(r: &QueryResult, col: usize) -> Vec<String> {
    r.rows
        .iter()
        .map(|row| match &row[col] {
            ResultValue::String(s) => s.clone(),
            ResultValue::Null => "null".into(),
            other => format!("{other:?}"),
        })
        .collect()
}

fn ints(r: &QueryResult, col: usize) -> Vec<i64> {
    r.rows
        .iter()
        .map(|row| match &row[col] {
            ResultValue::Int(i) => *i,
            other => panic!("not an int: {other:?}"),
        })
        .collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn spec_basic_node_queries() {
    let mut g = cyber();
    let r = run(&mut g, "MATCH (m:Malware) RETURN m.id, m.family, m.severity LIMIT 100;");
    assert_eq!(r.columns, vec!["m.id", "m.family", "m.severity"]);
    assert_eq!(r.rows.len(), 2);

    let r = run(
        &mut g,
        "MATCH (m:Malware) WHERE m.severity = 'high' RETURN m.id, m.family ORDER BY m.family ASC LIMIT 50;",
    );
    assert_eq!(strings(&r, 0), vec!["malware-001"]);

    let r = run(
        &mut g,
        "MATCH (h:Host) WHERE h.risk_score >= 0.80 AND h.hostname CONTAINS 'workstation' RETURN h.id, h.hostname, h.risk_score;",
    );
    assert_eq!(strings(&r, 0), vec!["host-001"]);
}

#[test]
fn spec_relationship_patterns() {
    let mut g = cyber();
    let r = run(
        &mut g,
        "MATCH (h:Host)-[e:DETECTED]->(m:Malware) RETURN h.hostname, e.confidence, m.family ORDER BY e.confidence DESC",
    );
    assert_eq!(strings(&r, 0), vec!["workstation-01", "server-01", "server-01"]);
    assert_eq!(r.rows[0][1], ResultValue::Float(0.97));

    let r = run(
        &mut g,
        "MATCH (h:Host)-[:DETECTED]->(m:Malware)\n -[:USES_INDICATOR]->(i:Indicator)\nWHERE h.id = 'host-001'\nRETURN h.hostname, m.family, i.value;",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(strings(&r, 2), vec!["example.invalid"]);

    // Reverse direction and undirected.
    let r = run(&mut g, "MATCH (m:Malware)<-[:DETECTED]-(h) WHERE m.id = 'malware-001' RETURN h.id ORDER BY h.id");
    assert_eq!(strings(&r, 0), vec!["host-001", "host-003"]);
    let r = run(&mut g, "MATCH (m {id: 'malware-001'})--(x) RETURN x.id ORDER BY x.id");
    assert_eq!(strings(&r, 0), vec!["host-001", "host-003", "ioc-001"]);
}

#[test]
fn spec_bounded_traversal_and_paths() {
    let mut g = cyber();
    let r = run(
        &mut g,
        "MATCH p = (h:Host)-[:DETECTED|USES_INDICATOR*1..3]->(x) WHERE h.id = 'host-001' RETURN x.id, length(p) ORDER BY length(p)",
    );
    assert_eq!(strings(&r, 0), vec!["malware-001", "ioc-001"]);
    assert_eq!(ints(&r, 1), vec![1, 2]);
    match &g.run("MATCH p = (h {id:'host-001'})-[*2]->(x) RETURN p").expect("path").rows[0][0] {
        ResultValue::Path(p) => {
            assert_eq!(
                p.nodes.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(),
                vec!["host-001", "malware-001", "ioc-001"]
            );
            assert_eq!(p.edges.len(), 2);
        }
        other => panic!("expected path, got {other:?}"),
    }
    // Unbounded traversal is rejected.
    assert!(matches!(g.run("MATCH (a)-[*]->(b) RETURN b"), Err(GraphError::Semantic(_))));
    assert!(matches!(g.run("MATCH (a)-[*1..1000]->(b) RETURN b"), Err(GraphError::LimitExceeded(_))));
    // Zero-length paths include the start node.
    let r = run(&mut g, "MATCH (h {id:'host-001'})-[*0..1]->(x) RETURN x.id ORDER BY x.id");
    assert_eq!(strings(&r, 0), vec!["host-001", "malware-001"]);
}

#[test]
fn traversal_terminates_on_cycles_with_trail_semantics() {
    let mut g = Graph::in_memory();
    for i in 0..4 {
        g.add_node(Node::new(format!("n{i}"))).expect("node");
    }
    for i in 0..4 {
        g.add_edge(Edge::new(format!("e{i}"), format!("n{i}"), format!("n{}", (i + 1) % 4), "NEXT")).expect("edge");
    }
    // The cycle has 4 edges; no path can be longer than that without reusing an edge.
    let r = run(&mut g, "MATCH (a {id:'n0'})-[r*1..10]->(b) RETURN size(r) AS len ORDER BY len");
    assert_eq!(ints(&r, 0), vec![1, 2, 3, 4]);
    let r = run(&mut g, "MATCH (a {id:'n0'})-[r*1..10]-(b) RETURN count(*) AS c");
    assert_eq!(ints(&r, 0), vec![8]);
}

#[test]
fn shortest_path_function_and_api() {
    let mut g = cyber();
    let r =
        run(&mut g, "MATCH p = shortestPath((a {id:'host-001'})-[*..5]-(b {id:'ioc-002'})) RETURN length(p) AS len");
    assert_eq!(ints(&r, 0), vec![4]);
    let r = run(&mut g, "MATCH p = shortestPath((a {id:'host-001'})-[*..2]-(b {id:'ioc-002'})) RETURN length(p)");
    assert!(r.rows.is_empty());
    let path = g
        .shortest_path("host-001", "ioc-002", &[], graphydra_core::Direction::Both, 10)
        .expect("ok")
        .expect("path exists");
    assert_eq!(path.len(), 4);
    assert_eq!(path.nodes.first().map(|n| n.id.as_str()), Some("host-001"));
    assert_eq!(path.nodes.last().map(|n| n.id.as_str()), Some("ioc-002"));
    assert!(
        g.shortest_path("host-001", "ioc-002", &[], graphydra_core::Direction::Outgoing, 10).expect("ok").is_none()
    );
}

#[test]
fn spec_mutations() {
    let mut g = cyber();
    let r = run(&mut g, "CREATE NODE (:Host {id: 'host-004', hostname: 'workstation-04', risk_score: 0.35});");
    assert_eq!(r.stats.nodes_created, 1);
    run(
        &mut g,
        "MATCH (h:Host), (m:Malware) WHERE h.id = 'host-004' AND m.id = 'malware-001' CREATE EDGE (h)-[:DETECTED {confidence: 0.88}]->(m);",
    );
    let r = run(&mut g, "MATCH (h:Host {id:'host-004'})-[e:DETECTED]->(m) RETURN e.confidence, m.id");
    assert_eq!(r.rows[0][0], ResultValue::Float(0.88));

    let r = run(&mut g, "MATCH (h:Host) WHERE h.id = 'host-004' SET h.risk_score = 0.72;");
    assert_eq!(r.stats.properties_set, 1);
    assert_eq!(g.get_node("host-004").expect("node").properties["risk_score"], Value::Float(0.72));

    let r = run(&mut g, "MATCH (h:Host)-[e:DETECTED]->(m:Malware) WHERE h.id = 'host-004' DELETE e;");
    assert_eq!(r.stats.edges_deleted, 1);

    // Node deletion policy: reject while edges exist, unless DETACH.
    assert!(matches!(g.run("MATCH (m {id:'malware-001'}) DELETE m"), Err(GraphError::NodeHasEdges { .. })));
    assert!(g.contains_node("malware-001"));
    let r = run(&mut g, "MATCH (m {id:'malware-001'}) DETACH DELETE m");
    assert_eq!((r.stats.nodes_deleted, r.stats.edges_deleted), (1, 3));
    assert!(g.verify().is_empty());
}

#[test]
fn create_returns_and_chains() {
    let mut g = Graph::in_memory();
    let r = run(
        &mut g,
        "CREATE (a:P {id:'a', n:1})-[r:K {id:'r1'}]->(b:P {id:'b', n:2}), (b)-[:K]->(c:P {id: 'c'}) RETURN a.id, r.id, b.n, c.id",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(strings(&r, 0), vec!["a"]);
    assert_eq!(g.edge_count(), 2);
    let r = run(&mut g, "MATCH (x:P) SET x.seen = true, x:Seen RETURN count(x)");
    assert_eq!(ints(&r, 0), vec![3]);
    assert_eq!(run(&mut g, "MATCH (x:Seen) RETURN count(*)").rows[0][0], ResultValue::Int(3));
    run(&mut g, "MATCH (x {id:'a'}) SET x += {k: 'v', n: null}");
    let a = g.get_node("a").expect("a");
    assert_eq!(a.properties.get("k"), Some(&Value::from("v")));
    assert!(!a.properties.contains_key("n"));
    run(&mut g, "MATCH (x {id:'a'}) SET x = {only: 1} REMOVE x:P");
    let a = g.get_node("a").expect("a");
    assert_eq!(a.properties.len(), 1);
    assert!(!a.labels.contains("P"));
    // Undirected edge creation.
    run(&mut g, "MATCH (a {id:'a'}), (c {id:'c'}) CREATE (a)-[:SIB]-(c)");
    let r = run(&mut g, "MATCH (c {id:'c'})-[:SIB]->(x) RETURN x.id");
    assert_eq!(strings(&r, 0), vec!["a"]);
}

#[test]
fn failed_statement_is_atomic() {
    let mut g = cyber();
    let before: Vec<Node> = g.nodes().collect();
    // Second CREATE collides with an existing id: nothing from this statement may remain.
    let err = g.run("CREATE (:X {id:'new-1'}), (:X {id:'host-001'})").expect_err("duplicate");
    assert!(matches!(err, GraphError::DuplicateId { .. }));
    assert_eq!(g.nodes().collect::<Vec<_>>(), before);
    // Also inside an explicit transaction: the statement rolls back, the tx stays open.
    g.begin().expect("begin");
    run(&mut g, "CREATE (:X {id:'ok'})");
    assert!(g.run("CREATE (:X {id:'bad'}) WITH_SYNTAX_ERROR").is_err());
    assert!(g.run("CREATE (:X {id:'bad2'}), (:X {id:'ok'})").is_err());
    assert!(g.contains_node("ok") && !g.contains_node("bad2"));
    g.rollback().expect("rollback");
    assert!(!g.contains_node("ok"));
    assert!(g.verify().is_empty());
}

#[test]
fn parameters_prevent_injection() {
    let mut g = cyber();
    let evil = "host-001' OR 1=1 OR h.id = '";
    let r = g
        .query(
            "MATCH (h:Host) WHERE h.id = $host_id RETURN h.hostname, h.risk_score LIMIT 10",
            &QueryOptions::new().param("host_id", evil),
        )
        .expect("query");
    assert!(r.rows.is_empty());
    let r = g
        .query(
            "MATCH (h:Host) WHERE h.id = $host_id RETURN h.hostname",
            &QueryOptions::new().param("host_id", "host-001"),
        )
        .expect("query");
    assert_eq!(strings(&r, 0), vec!["workstation-01"]);
    assert!(matches!(g.run("MATCH (h) WHERE h.id = $missing RETURN h"), Err(GraphError::Parameter(_))));
    let r = g
        .query(
            "MATCH (h:Host) WHERE h.id IN $ids RETURN h.id ORDER BY h.id",
            &QueryOptions::new().param("ids", vec!["host-002", "host-003"]),
        )
        .expect("in");
    assert_eq!(strings(&r, 0), vec!["host-002", "host-003"]);
}

#[test]
fn ordering_pagination_and_determinism() {
    let mut g = cyber();
    let q = "MATCH (n) RETURN n.id ORDER BY n.id DESC SKIP 1 LIMIT 3";
    let first = run(&mut g, q);
    assert_eq!(strings(&first, 0), vec!["malware-001", "ioc-002", "ioc-001"]);
    for _ in 0..5 {
        assert_eq!(run(&mut g, q).rows, first.rows);
    }
    // Nulls sort last ascending, first descending.
    let r = run(&mut g, "MATCH (n) RETURN n.id, n.risk_score AS r ORDER BY r, n.id");
    assert_eq!(r.rows.last().map(|row| row[1].clone()), Some(ResultValue::Null));
    let r = run(&mut g, "MATCH (n:Host) RETURN n.id ORDER BY n.risk_score DESC LIMIT 2");
    assert_eq!(strings(&r, 0), vec!["host-001", "host-003"]);
    // Top-k path with many rows.
    let mut big = Graph::in_memory();
    big.add_nodes((0..5000).map(|i| Node::new(format!("n{i:05}")).with_property("v", (i * 7919) % 5000)))
        .expect("bulk");
    let r = run(&mut big, "MATCH (n) RETURN n.v ORDER BY n.v DESC SKIP 10 LIMIT 5");
    assert_eq!(ints(&r, 0), vec![4989, 4988, 4987, 4986, 4985]);
}

#[test]
fn aggregation_and_grouping() {
    let mut g = cyber();
    let r = run(
        &mut g,
        "MATCH (h:Host)-[e:DETECTED]->(m) RETURN h.id, count(m) AS n, avg(e.confidence) AS c ORDER BY n DESC, h.id",
    );
    assert_eq!(strings(&r, 0), vec!["host-003", "host-001"]);
    assert_eq!(ints(&r, 1), vec![2, 1]);
    let r = run(&mut g, "MATCH (n) RETURN labels(n)[0] AS l, count(*) AS c ORDER BY c DESC, l");
    assert_eq!(strings(&r, 0), vec!["Host", "Indicator", "Malware"]);
    let r = run(&mut g, "MATCH (n:Nothing) RETURN count(*), sum(n.x), collect(n.id), avg(n.x), max(n.x)");
    assert_eq!(
        r.rows[0],
        vec![ResultValue::Int(0), ResultValue::Int(0), ResultValue::List(vec![]), ResultValue::Null, ResultValue::Null]
    );
    let r = run(&mut g, "MATCH (h:Host)-[:DETECTED]->(m) RETURN count(DISTINCT m) AS d, count(m) AS all");
    assert_eq!(r.rows[0], vec![ResultValue::Int(2), ResultValue::Int(3)]);
    let r = run(&mut g, "MATCH (n:Host) RETURN min(n.risk_score), max(n.hostname), sum(n.risk_score) > 2");
    assert_eq!(r.rows[0][0], ResultValue::Float(0.35));
    assert_eq!(r.rows[0][1], ResultValue::String("workstation-02".into()));
    let r = run(&mut g, "MATCH (n) RETURN DISTINCT labels(n) AS l ORDER BY l");
    assert_eq!(r.rows.len(), 3);
    assert!(g.run("MATCH (n) RETURN n.id, n.x + count(*)").is_err());
    assert!(g.run("MATCH (n) WHERE count(n) > 1 RETURN n").is_err());
}

#[test]
fn expression_semantics() {
    let mut g = Graph::in_memory();
    let r = run(
        &mut g,
        "RETURN null = null AS a, null <> 1 AS b, NOT null AS c, true OR null AS d, false AND null AS e, \
         null IS NULL AS f, 1 IN [1, null] AS g, 2 IN [1, null] AS h, 1 = 1.0 AS i, '1' = 1 AS j, \
         3 / 2 AS k, 3.0 / 2 AS l, 7 % 3 AS m, 2 ^ 10 AS n, 'a' + 1 AS o, [1] + [2] AS p, \
         'abc' STARTS WITH 'ab' AS q, 'abc' ENDS WITH 'bc' AS r, 1 < 'a' AS s, \
         CASE WHEN 1 > 2 THEN 'x' ELSE 'y' END AS t, coalesce(null, 3) AS u, size('héllo') AS v, \
         toInteger('42') AS w, substring('abcdef', 1, 3) AS x, [1,2,3][-1] AS y, range(1, 5, 2) AS z",
    );
    use ResultValue::*;
    let expect = vec![
        Null,
        Null,
        Null,
        Bool(true),
        Bool(false),
        Bool(true),
        Bool(true),
        Null,
        Bool(true),
        Bool(false),
        Int(1),
        Float(1.5),
        Int(1),
        Float(1024.0),
        String("a1".into()),
        List(vec![Int(1), Int(2)]),
        Bool(true),
        Bool(true),
        Null,
        String("y".into()),
        Int(3),
        Int(5),
        Int(42),
        String("bcd".into()),
        Int(3),
        List(vec![Int(1), Int(3), Int(5)]),
    ];
    assert_eq!(r.rows[0], expect);
    for bad in ["RETURN 1 / 0", "RETURN 9223372036854775807 + 1", "RETURN 1 + true", "RETURN unknown_fn(1)", "RETURN x"]
    {
        assert!(g.run(bad).is_err(), "{bad} should fail");
    }
}

#[test]
fn indexes_are_used_and_equivalent() {
    let mut g = cyber();
    let q = "MATCH (h:Host) WHERE h.hostname = 'server-01' RETURN h.id";
    let plan = g.explain(q, &QueryOptions::new()).expect("plan");
    assert!(plan.to_text().contains("LabelScan"), "{}", plan.to_text());
    assert!(plan.notes.iter().any(|n| n.contains("no index on nodes(hostname)")));
    let before = run(&mut g, q);

    g.create_index(IndexDef::new(IndexKind::Hash, IndexTarget::Node, "hostname")).expect("index");
    g.create_index(IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "risk_score")).expect("index");
    g.create_index(IndexDef::new(IndexKind::Text, IndexTarget::Node, "hostname")).expect("index");
    let plan = g.explain(q, &QueryOptions::new()).expect("plan");
    assert!(plan.to_text().contains("IndexSeek hash index on nodes(hostname)"), "{}", plan.to_text());
    assert_eq!(run(&mut g, q).rows, before.rows);

    let id_plan = g
        .explain("MATCH (h:Host)-[:DETECTED]->(m) WHERE h.id = 'host-001' RETURN m.family", &QueryOptions::new())
        .expect("plan");
    assert!(id_plan.to_text().contains("via IdLookup h.id = \"host-001\""), "{}", id_plan.to_text());

    let queries = [
        "MATCH (h:Host) WHERE h.risk_score > 0.5 RETURN h.id ORDER BY h.id",
        "MATCH (h:Host) WHERE h.risk_score >= 0.35 AND h.risk_score < 0.9 RETURN h.id ORDER BY h.id",
        "MATCH (h:Host) WHERE h.hostname STARTS WITH 'work' RETURN h.id ORDER BY h.id",
        "MATCH (h) WHERE text_match(h.hostname, 'SERVER') RETURN h.id",
        "MATCH (h) WHERE h.hostname IN ['server-01', 'nope', null] RETURN h.id",
        "MATCH (h) WHERE h.risk_score = 1 RETURN h.id",
        "MATCH (h) WHERE h.hostname = null RETURN h.id",
        "MATCH (h) WHERE 0.5 < h.risk_score RETURN h.id ORDER BY h.id",
        "MATCH ()-[e:DETECTED]->() WHERE e.id = 'edge-003' RETURN e.confidence",
    ];
    for q in queries {
        let indexed = run(&mut g, q);
        let plain = g.query(q, &QueryOptions { disable_indexes: true, ..QueryOptions::default() }).expect("plain");
        assert_eq!(indexed.rows, plain.rows, "{q}");
    }
    let range_plan = g.explain(queries[1], &QueryOptions::new()).expect("plan").to_text();
    assert!(range_plan.contains("IndexRangeScan"), "{range_plan}");

    let hits = g.search_text("server", None, 10).expect("search");
    assert_eq!(hits.first().map(|h| h.0.id.as_str()), Some("host-003"));
    assert!(g.verify().is_empty());
}

#[test]
fn ddl_and_show_indexes() {
    let mut g = cyber();
    run(&mut g, "CREATE SORTED INDEX ON NODES(risk_score)");
    run(&mut g, "CREATE TEXT INDEX ON EDGES(note)");
    let r = run(&mut g, "SHOW INDEXES");
    assert_eq!(r.rows.len(), 2);
    assert!(g.run("CREATE SORTED INDEX ON NODES(risk_score)").is_err());
    run(&mut g, "DROP SORTED INDEX ON NODES(risk_score)");
    assert_eq!(g.indexes().len(), 1);
}

#[test]
fn read_only_mode_and_limits() {
    let mut g = cyber();
    let ro = QueryOptions::new().read_only();
    assert!(matches!(g.query("MATCH (n) DETACH DELETE n", &ro), Err(GraphError::ReadOnly)));
    assert!(matches!(g.query("CREATE INDEX ON NODES(x)", &ro), Err(GraphError::ReadOnly)));
    assert!(g.query("MATCH (n) RETURN count(n)", &ro).is_ok());
    // EXPLAIN never executes, even for mutations.
    assert!(g.query("EXPLAIN MATCH (n) DETACH DELETE n", &ro).is_ok());
    assert_eq!(g.node_count(), 7);

    let limited = QueryOptions { max_rows: Some(3), ..QueryOptions::default() };
    assert!(matches!(g.query("MATCH (n) RETURN n", &limited), Err(GraphError::LimitExceeded(_))));
    let budget = QueryOptions { max_visits: Some(5), ..QueryOptions::default() };
    assert!(matches!(g.query("MATCH (a)-->(b) RETURN a", &budget), Err(GraphError::LimitExceeded(_))));

    let token = CancellationToken::new();
    token.cancel();
    let cancelled = QueryOptions { cancellation: Some(token), ..QueryOptions::default() };
    assert!(matches!(g.query("MATCH (n) RETURN n", &cancelled), Err(GraphError::Cancelled)));
}

#[test]
fn timeout_is_enforced() {
    let mut g = Graph::in_memory();
    g.add_nodes((0..300).map(|i| Node::new(format!("n{i}")))).expect("nodes");
    let opts = QueryOptions::new().timeout(Duration::from_millis(50));
    let start = std::time::Instant::now();
    let r = g.query("MATCH (a), (b), (c) RETURN count(*)", &opts);
    assert!(matches!(r, Err(GraphError::Timeout)), "{r:?}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn semantic_errors_are_actionable() {
    let mut g = cyber();
    for (q, needle) in [
        ("MATCH (n) RETURN m", "`m` is not defined"),
        ("MATCH (n)-[r]->(m), (a)-[r]->(b) RETURN n", "bound more than once"),
        ("MATCH (n)-[n]->(m) RETURN n", "both"),
        ("MATCH (n) SET n.id = 'x'", "immutable"),
        ("CREATE (a)-[:A|B]->(b)", "exactly one relationship type"),
        ("MATCH (n) RETURN n LIMIT -1", "non-negative"),
        ("MATCH (n) RETURN n.id AS x, n.hostname AS x", "duplicate column"),
    ] {
        let err = g.run(q).expect_err(q).to_string();
        assert!(err.contains(needle), "{q}: {err}");
    }
    match g.run("MATCH (n)\nWHERE n.x = \nRETURN n") {
        Err(GraphError::Syntax(e)) => assert_eq!((e.line, e.column), (3, 1)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn profile_reports_statistics() {
    let mut g = cyber();
    let r = g.run("PROFILE MATCH (h:Host)-[:DETECTED]->(m) WHERE h.id = 'host-003' RETURN m.id").expect("profile");
    assert_eq!(r.rows.len(), 2);
    assert!(r.plan.is_some());
    assert_eq!(r.stats.nodes_scanned, 1);
    assert_eq!(r.stats.edges_traversed, 2);
    assert!(r.stats.index_lookups >= 1);
    let r2 = g.run("PROFILE MATCH (h:Host)-[:DETECTED]->(m) WHERE h.id = 'host-003' RETURN m.id").expect("profile");
    assert!(r2.stats.statement_cache_hit);
}

#[test]
fn find_nodes_and_neighbors() {
    let mut g = cyber();
    let props = [("severity".to_owned(), Value::from("high"))].into_iter().collect();
    let found = g.find_nodes(Some("Malware"), &props, None).expect("find");
    assert_eq!(found.iter().map(|n| n.id.as_str()).collect::<Vec<_>>(), vec!["malware-001"]);
    let all_hosts = g.find_nodes(Some("Host"), &Default::default(), Some(2)).expect("find");
    assert_eq!(all_hosts.len(), 2);
    let n = g.neighbors("malware-001", graphydra_core::Direction::Incoming, Some("DETECTED")).expect("neighbors");
    assert_eq!(sorted(n.iter().map(|(_, n)| n.id.clone()).collect()), vec!["host-001", "host-003"]);
}

#[test]
fn optimizer_regressions() {
    let mut g = cyber();
    g.create_index(IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "risk_score")).expect("index");
    // Constant folding lets both bounds reach the range index.
    let plan = g
        .explain(
            "MATCH (h) WHERE h.risk_score >= $p AND h.risk_score < $p + 0.1 RETURN h",
            &QueryOptions::new().param("p", 0.3),
        )
        .expect("plan")
        .to_text();
    assert!(plan.contains("IndexRangeScan") && plan.contains("(est. 1)"), "{plan}");
    // A selective shortest-path target is bound before the search.
    let plan = g
        .explain("MATCH p = shortestPath((a {id:'host-001'})-[*..5]-(b {id:'ioc-002'})) RETURN p", &QueryOptions::new())
        .expect("plan")
        .to_text();
    assert!(plan.contains("shortest-path target") && plan.contains("[bound target]"), "{plan}");
    // Folding never hides runtime errors.
    assert!(g.run("RETURN 1 / 0").is_err());
    assert!(g.run("MATCH (n:Nope) RETURN 1 / 0").expect("no rows, no error").rows.is_empty());
}
