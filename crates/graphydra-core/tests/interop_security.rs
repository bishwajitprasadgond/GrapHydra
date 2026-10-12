//! Import/export round trips (FR-14) and security behaviour (spec section 12).

use std::time::Duration;

use graphydra_core::interop::{
    export_csv, export_graphml, export_json, export_jsonl, import_csv, import_json, import_jsonl,
};
use graphydra_core::{Config, Edge, Graph, GraphError, Node, QueryOptions, Value};

fn sample() -> Graph {
    let mut g = Graph::in_memory();
    g.add_node(
        Node::new("a")
            .with_label("Host")
            .with_label("Asset")
            .with_property("name", "alpha, \"one\"")
            .with_property("score", 0.5)
            .with_property("n", 3)
            .with_property("ok", true)
            .with_property("tags", vec!["x", "y"]),
    )
    .expect("a");
    g.add_node(Node::new("b").with_label("Host").with_property("name", "<b & c>").with_property("score", 2))
        .expect("b");
    g.add_edge(Edge::new("e1", "a", "b", "LINKS").with_property("w", 1)).expect("e1");
    g.add_edge(Edge::new("e2", "b", "a", "PEER").undirected()).expect("e2");
    g
}

fn same(a: &Graph, b: &Graph) -> bool {
    a.nodes().collect::<Vec<_>>() == b.nodes().collect::<Vec<_>>()
        && a.edges().collect::<Vec<_>>() == b.edges().collect::<Vec<_>>()
}

#[test]
fn json_and_jsonl_round_trip() {
    let g = sample();
    let mut buf = Vec::new();
    export_json(&g, &mut buf).expect("export");
    let mut h = Graph::in_memory();
    assert_eq!(import_json(&mut h, buf.as_slice()).expect("import").nodes, 2);
    assert!(same(&g, &h));

    let mut buf = Vec::new();
    export_jsonl(&g, &mut buf).expect("export");
    let mut h = Graph::in_memory();
    import_jsonl(&mut h, buf.as_slice()).expect("import");
    assert!(same(&g, &h));
}

#[test]
fn csv_round_trip_preserves_types() {
    let g = sample();
    let (mut nodes, mut edges) = (Vec::new(), Vec::new());
    export_csv(&g, &mut nodes, &mut edges).expect("export");
    let header = String::from_utf8(nodes.clone()).expect("utf8");
    assert!(header.starts_with("id,labels,n:int,name:string,ok:bool,score:float,tags:json"), "{header}");
    let mut h = Graph::in_memory();
    import_csv(&mut h, Some(nodes.as_slice()), Some(edges.as_slice())).expect("import");
    let b = h.get_node("b").expect("b");
    // Integers in a float column come back as floats, which compare equal in queries.
    assert_eq!(b.properties["score"], Value::Float(2.0));
    let a = h.get_node("a").expect("a");
    assert_eq!(a.properties["tags"], Value::from(vec!["x", "y"]));
    assert_eq!(a.properties["name"], Value::from("alpha, \"one\""));
    assert!(!h.get_edge("e2").expect("e2").directed);
}

#[test]
fn imports_are_atomic_and_strict() {
    let mut g = Graph::in_memory();
    let bad = "{\"kind\":\"node\",\"id\":\"x\"}\n{\"kind\":\"edge\",\"source\":\"x\",\"target\":\"missing\",\"type\":\"T\"}\n";
    assert!(matches!(import_jsonl(&mut g, bad.as_bytes()), Err(GraphError::DanglingEdge { .. })));
    assert_eq!(g.node_count(), 0, "nothing imported after a failure");
    let unknown = r#"{"nodes":[{"id":"x","lables":["typo"]}]}"#;
    assert!(matches!(import_json(&mut g, unknown.as_bytes()), Err(GraphError::Format(_))));
    let csv = "id,score:float\nx,not-a-number\n";
    assert!(import_csv(&mut g, Some(csv.as_bytes()), None::<&[u8]>).is_err());
    let edges = "source,target,type\n";
    // Edges without ids get generated ones.
    let nodes = "id,labels\np,A\nq,B\n";
    let edges = format!("{edges}p,q,T\np,q,T\n");
    import_csv(&mut g, Some(nodes.as_bytes()), Some(edges.as_bytes())).expect("csv");
    assert_eq!(g.edge_count(), 2);
}

#[test]
fn graphml_is_escaped() {
    let g = sample();
    let mut buf = Vec::new();
    export_graphml(&g, &mut buf).expect("export");
    let xml = String::from_utf8(buf).expect("utf8");
    assert!(xml.contains("&lt;b &amp; c&gt;"));
    assert!(xml.contains(r#"<edge id="e2" source="b" target="a" directed="false">"#));
    assert!(!xml.contains("<b & c>"));
}

#[test]
fn query_language_has_no_escape_hatches() {
    let mut g = sample();
    // No code evaluation, file access, imports or procedures exist in the grammar.
    for q in [
        "CALL dbms.procedures()",
        "LOAD CSV FROM 'file:///etc/passwd' AS line RETURN line",
        "RETURN eval('1+1')",
        "RETURN __import__('os')",
        "MATCH (n) RETURN n; DROP TABLE x",
        "RETURN apoc.load.json('http://example.invalid')",
    ] {
        assert!(g.run(q).is_err(), "{q} must be rejected");
    }
    // Strings are data, even when they look like queries.
    let r = g.query("RETURN $x AS x", &QueryOptions::new().param("x", "MATCH (n) DETACH DELETE n")).expect("ok");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(g.node_count(), 2);
}

#[test]
fn pathological_queries_fail_predictably() {
    let mut g = Graph::in_memory();
    // A dense clique makes variable-length expansion explode combinatorially.
    let n = 12;
    g.add_nodes((0..n).map(|i| Node::new(format!("n{i}")))).expect("nodes");
    let mut edges = Vec::new();
    for i in 0..n {
        for j in 0..n {
            if i != j {
                edges.push(Edge::new(format!("e{i}-{j}"), format!("n{i}"), format!("n{j}"), "K"));
            }
        }
    }
    g.add_edges(edges).expect("edges");
    let budgeted = QueryOptions { max_visits: Some(100_000), ..QueryOptions::default() };
    assert!(matches!(g.query("MATCH (a)-[*1..12]->(b) RETURN count(*)", &budgeted), Err(GraphError::LimitExceeded(_))));
    let timed = QueryOptions::new().timeout(Duration::from_millis(100));
    assert!(matches!(g.query("MATCH (a)-[*1..12]->(b) RETURN count(*)", &timed), Err(GraphError::Timeout)));
    assert!(matches!(g.run("RETURN range(1, 100000000)"), Err(GraphError::LimitExceeded(_))));

    let mut config = Config::default();
    config.limits.max_query_bytes = 64;
    let mut small = Graph::in_memory_with(config);
    assert!(matches!(small.run(&format!("RETURN '{}'", "x".repeat(100))), Err(GraphError::LimitExceeded(_))));
    // Deeply nested expressions are bounded by the query size limit, not the stack.
    let deep = format!("RETURN {}1{}", "(".repeat(5000), ")".repeat(5000));
    let mut config = Config::default();
    config.limits.max_query_bytes = 4096;
    let mut g2 = Graph::in_memory_with(config);
    assert!(g2.run(&deep).is_err());
}

#[test]
fn property_values_are_validated() {
    let mut g = Graph::in_memory();
    assert!(g.add_node(Node::new("x").with_property("bad", f64::NAN)).is_err());
    assert!(g.add_node(Node::new("x").with_property("id", "dup")).is_err());
    assert!(g.add_node(Node::new("ctl\u{0}")).is_err());
    let mut config = Config::default();
    config.limits.max_string_bytes = 10;
    let mut g = Graph::in_memory_with(config);
    assert!(g.add_node(Node::new("x").with_property("s", "01234567890")).is_err());
    assert!(g.run("CREATE (:A {id: 'y', v: 1.0 / 0})").is_err());
    assert_eq!(g.node_count(), 0);
}
