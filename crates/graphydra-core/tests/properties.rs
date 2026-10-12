//! Property-based tests of the correctness invariants in spec section 13.2.

use std::collections::BTreeSet;

use graphydra_core::{
    Config, Durability, Edge, Graph, IndexDef, IndexKind, IndexTarget, Node, OpenOptions, QueryOptions, ResultValue,
    Value,
};
use proptest::prelude::*;

#[derive(Debug, Clone)]
struct GenGraph {
    nodes: Vec<(Vec<&'static str>, Option<Value>, Option<&'static str>)>,
    edges: Vec<(usize, usize, &'static str, bool, Option<i64>)>,
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        (-3i64..4).prop_map(Value::Int),
        (-3i64..4).prop_map(|i| Value::Float(i as f64 / 2.0)),
        prop::sample::select(vec!["a", "b", "ab", "B"]).prop_map(Value::from),
        Just(Value::Bool(true)),
        Just(Value::List(vec![Value::Int(1)])),
    ]
}

fn graph_strategy() -> impl Strategy<Value = GenGraph> {
    let node = (
        prop::sample::subsequence(vec!["A", "B", "C"], 0..=2),
        prop::option::of(value()),
        prop::option::of(prop::sample::select(vec!["red fox", "Red", "blue fox jumps", "green"])),
    );
    (prop::collection::vec(node, 1..14)).prop_flat_map(|nodes| {
        let n = nodes.len();
        let edge = (0..n, 0..n, prop::sample::select(vec!["T", "U"]), any::<bool>(), prop::option::of(0i64..3));
        (Just(nodes), prop::collection::vec(edge, 0..24)).prop_map(|(nodes, edges)| GenGraph { nodes, edges })
    })
}

fn build(spec: &GenGraph) -> Graph {
    let mut g = Graph::in_memory();
    for (i, (labels, p, text)) in spec.nodes.iter().enumerate() {
        let mut n = Node::new(format!("n{i}"));
        for l in labels {
            n = n.with_label(*l);
        }
        if let Some(p) = p {
            n.properties.insert("p".into(), p.clone());
        }
        if let Some(t) = text {
            n.properties.insert("t".into(), Value::from(*t));
        }
        g.add_node(n).expect("node");
    }
    for (i, (s, t, ty, directed, w)) in spec.edges.iter().enumerate() {
        let mut e = Edge::new(format!("e{i}"), format!("n{s}"), format!("n{t}"), *ty);
        e.directed = *directed;
        if let Some(w) = w {
            e.properties.insert("w".into(), Value::Int(*w));
        }
        g.add_edge(e).expect("edge");
    }
    g
}

fn add_indexes(g: &mut Graph) {
    for def in [
        IndexDef::new(IndexKind::Hash, IndexTarget::Node, "p"),
        IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "p"),
        IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "t"),
        IndexDef::new(IndexKind::Text, IndexTarget::Node, "t"),
        IndexDef::new(IndexKind::Hash, IndexTarget::Edge, "w"),
    ] {
        g.create_index(def).expect("index");
    }
}

fn query_templates() -> impl Strategy<Value = String> {
    let op = prop::sample::select(vec!["=", "<>", "<", "<=", ">", ">="]);
    let lit = prop::sample::select(vec!["1", "0.5", "-1", "'a'", "'ab'", "true", "null", "[1]"]);
    let label = prop::sample::select(vec!["", ":A", ":B", ":A:B"]);
    let dir = prop::sample::select(vec![("-", "->"), ("<-", "-"), ("-", "-")]);
    prop_oneof![
        (label.clone(), op.clone(), lit.clone())
            .prop_map(|(l, o, v)| format!("MATCH (n{l}) WHERE n.p {o} {v} RETURN n.id")),
        (label.clone(), lit.clone()).prop_map(|(l, v)| format!("MATCH (n{l}) WHERE {v} = n.p RETURN n.id")),
        (op.clone(), lit.clone(), op.clone(), lit.clone())
            .prop_map(|(o1, v1, o2, v2)| format!("MATCH (n) WHERE n.p {o1} {v1} AND n.p {o2} {v2} RETURN n.id")),
        lit.clone().prop_map(|v| format!("MATCH (n) WHERE n.p IN [{v}, 2, 'b'] RETURN n.id")),
        prop::sample::select(vec!["'r'", "'Red'", "'b'", "''"])
            .prop_map(|v| format!("MATCH (n) WHERE n.t STARTS WITH {v} RETURN n.id")),
        prop::sample::select(vec!["'fox'", "'RED FOX'", "'jumps blue'", "'nothing'", "''"])
            .prop_map(|v| format!("MATCH (n) WHERE text_match(n.t, {v}) RETURN n.id")),
        (label.clone(), dir.clone(), prop::sample::select(vec!["", ":T", ":U", ":T|U"]), lit.clone()).prop_map(
            |(l, (a, b), t, v)| format!(
                "MATCH (x{l}){a}[e{t}]{b}(y) WHERE x.p = {v} OR y.p = {v} RETURN x.id, e.id, y.id"
            )
        ),
        (dir.clone(), 0u32..3, 1u32..4).prop_map(|((a, b), lo, hi)| format!(
            "MATCH (x)<-[]-()-[]->(z), (x){a}[r*{lo}..{}]{b}(y) WHERE x.id = 'n0' RETURN y.id, size(r), z.id",
            lo.max(hi)
        )),
        prop::sample::select(vec!["0", "1", "2"])
            .prop_map(|w| format!("MATCH (a)-[e {{w: {w}}}]-(b) RETURN a.id, b.id, e.id")),
        Just("MATCH (n) RETURN labels(n) AS l, count(*) AS c, collect(n.id) AS ids".to_owned()),
        Just("MATCH (n:A)--(m:B) RETURN DISTINCT n.id".to_owned()),
        Just("MATCH p = shortestPath((a {id:'n0'})-[*..5]-(b)) RETURN b.id, length(p)".to_owned()),
    ]
}

fn sort_rows(mut rows: Vec<Vec<ResultValue>>) -> Vec<String> {
    let mut out: Vec<String> = rows.drain(..).map(|r| format!("{r:?}")).collect();
    out.sort();
    out
}

fn ids(rows: &[Vec<ResultValue>]) -> BTreeSet<String> {
    rows.iter()
        .map(|r| match &r[0] {
            ResultValue::String(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 200, ..ProptestConfig::default() })]

    /// Invariants 3 & 4: indexed and unindexed execution return the same
    /// rows, and so does a graph whose indexes were rebuilt.
    #[test]
    fn indexed_equals_unindexed(spec in graph_strategy(), q in query_templates()) {
        let mut plain = build(&spec);
        let mut indexed = build(&spec);
        add_indexes(&mut indexed);
        let a = plain.run(&q);
        let b = indexed.run(&q);
        let c = indexed.query(&q, &QueryOptions { disable_indexes: true, ..QueryOptions::default() });
        match (a, b, c) {
            (Ok(a), Ok(b), Ok(c)) => {
                prop_assert_eq!(sort_rows(a.rows.clone()), sort_rows(b.rows.clone()), "query {}", q);
                prop_assert_eq!(sort_rows(a.rows), sort_rows(c.rows), "query {}", q);
            }
            (a, b, c) => prop_assert!(false, "divergent outcome for {}: {:?} / {:?} / {:?}", q, a.err(), b.err(), c.err()),
        }
        indexed.rebuild_indexes();
        prop_assert!(indexed.verify().is_empty());
    }

    /// Filters agree with a brute-force evaluation of the same predicate.
    #[test]
    fn filters_match_oracle(spec in graph_strategy(), lit in value(), op in prop::sample::select(vec!["=", "<", ">="])) {
        let mut g = build(&spec);
        add_indexes(&mut g);
        let opts = QueryOptions::new().param("v", lit.clone());
        let r = g.query(&format!("MATCH (n:A) WHERE n.p {op} $v RETURN n.id"), &opts).expect("query");
        let expected: BTreeSet<String> = spec.nodes.iter().enumerate().filter(|(_, (labels, p, _))| {
            labels.contains(&"A") && p.as_ref().is_some_and(|p| match op {
                "=" => p.query_eq(&lit) == Some(true),
                "<" => p.query_cmp(&lit).is_some_and(|o| o.is_lt()),
                _ => p.query_cmp(&lit).is_some_and(|o| o.is_ge()),
            })
        }).map(|(i, _)| format!("n{i}")).collect();
        prop_assert_eq!(ids(&r.rows), expected);
    }

    /// One-hop expansion agrees with brute force over the edge list.
    #[test]
    fn expansion_matches_oracle(spec in graph_strategy()) {
        let mut g = build(&spec);
        let r = g.run("MATCH (a)-[e:T]->(b) RETURN e.id, a.id, b.id").expect("query");
        let got: BTreeSet<String> = r.rows.iter().map(|row| format!("{:?}", row)).collect();
        let mut expected = BTreeSet::new();
        for (i, (s, t, ty, directed, _)) in spec.edges.iter().enumerate() {
            if *ty != "T" { continue; }
            let row = |x: usize, y: usize| format!("{:?}", vec![
                ResultValue::String(format!("e{i}")), ResultValue::String(format!("n{x}")), ResultValue::String(format!("n{y}"))]);
            expected.insert(row(*s, *t));
            if !directed && s != t { expected.insert(row(*t, *s)); }
        }
        prop_assert_eq!(got, expected);
    }

    /// Invariant 2: a rolled-back transaction leaves no trace.
    #[test]
    fn rollback_is_exact(spec in graph_strategy(), victim in 0usize..14, value in value()) {
        let mut g = build(&spec);
        add_indexes(&mut g);
        let nodes: Vec<Node> = g.nodes().collect();
        let edges: Vec<Edge> = g.edges().collect();
        let id = format!("n{}", victim % spec.nodes.len());
        g.begin().expect("begin");
        let _ = g.run("MATCH (n) SET n.p = 42, n:Z");
        let _ = g.set_node_property(&id, "p", value);
        let _ = g.delete_node(&id, true);
        let _ = g.run("CREATE (:New {id: 'fresh'})-[:T]->(:New)");
        let _ = g.run("MATCH ()-[e:U]-() DELETE e");
        g.rollback().expect("rollback");
        prop_assert_eq!(g.nodes().collect::<Vec<_>>(), nodes);
        prop_assert_eq!(g.edges().collect::<Vec<_>>(), edges);
        prop_assert!(g.verify().is_empty());
    }

    /// Invariants 5 & 8: journal replay and snapshot reload reproduce the
    /// same logical graph; deleted nodes are unreachable.
    #[test]
    fn persistence_round_trip(spec in graph_strategy(), deletes in prop::collection::vec(0usize..14, 0..4)) {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("p.graph");
        let config = Config { durability: Durability::NoSync, ..Config::default() };
        let mut g = Graph::open_with(&path, OpenOptions::new().create(true).config(config.clone())).expect("create");
        let source = build(&spec);
        g.add_nodes(source.nodes()).expect("nodes");
        g.add_edges(source.edges()).expect("edges");
        add_indexes(&mut g);
        for d in &deletes {
            let _ = g.delete_node(&format!("n{}", d % spec.nodes.len()), true);
        }
        let nodes: Vec<Node> = g.nodes().collect();
        let edges: Vec<Edge> = g.edges().collect();
        drop(g); // no checkpoint: state lives in the journal
        let g = Graph::open_with(&path, OpenOptions::new().config(config.clone())).expect("replay");
        prop_assert_eq!(g.nodes().collect::<Vec<_>>(), nodes.clone());
        prop_assert_eq!(g.edges().collect::<Vec<_>>(), edges.clone());
        g.close().expect("checkpoint");
        let mut g = Graph::open_with(&path, OpenOptions::new().config(config)).expect("snapshot");
        prop_assert_eq!(g.nodes().collect::<Vec<_>>(), nodes);
        prop_assert_eq!(g.edges().collect::<Vec<_>>(), edges);
        prop_assert!(g.verify().is_empty());
        for d in &deletes {
            let id = format!("n{}", d % spec.nodes.len());
            let r = g.query("MATCH (n)--(m) WHERE m.id = $id RETURN n", &QueryOptions::new().param("id", id)).expect("q");
            prop_assert!(r.rows.is_empty());
        }
    }

    /// Invariant 6: a failed parse never mutates the graph.
    #[test]
    fn garbage_queries_never_mutate(spec in graph_strategy(), q in "[ -~]{0,60}") {
        let mut g = build(&spec);
        let nodes: Vec<Node> = g.nodes().collect();
        let generation = g.generation();
        if g.run(&q).is_err() {
            prop_assert_eq!(g.generation(), generation);
            prop_assert_eq!(g.nodes().collect::<Vec<_>>(), nodes);
        }
    }
}
