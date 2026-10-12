//! End-to-end tests of the `graphydra` binary (spec section 11).

use std::path::Path;
use std::process::{Command, Output};

fn gv(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_graphydra")).current_dir(dir).args(args).output().expect("run graphydra")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = gv(dir, args);
    assert!(
        out.status.success(),
        "graphydra {args:?} failed ({:?}):\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

#[test]
fn full_workflow() {
    let dir = tempfile::tempdir().expect("tmp");
    let d = dir.path();
    ok(d, &["init", "cyber.graph"]);
    assert_eq!(gv(d, &["init", "cyber.graph"]).status.code(), Some(2), "init refuses to overwrite");

    std::fs::write(
        d.join("input.json"),
        r#"{"nodes":[{"id":"host-001","labels":["Host"],"properties":{"hostname":"ws-01","risk_score":0.92}},
                     {"id":"malware-001","labels":["Malware"],"properties":{"family":"ExampleFamily"}}],
            "edges":[{"id":"edge-001","source":"host-001","target":"malware-001","type":"DETECTED","properties":{"confidence":0.97}}]}"#,
    )
    .expect("write");
    let out = ok(d, &["import", "cyber.graph", "--format", "json", "input.json"]);
    assert!(out.contains("imported 2 node(s) and 1 edge(s)"), "{out}");

    let out = ok(d, &["validate", "cyber.graph"]);
    assert!(out.contains("valid: 2 node(s), 1 edge(s)"), "{out}");

    let out = ok(d, &["info", "cyber.graph"]);
    assert!(out.contains(":Host") && out.contains(":DETECTED"), "{out}");

    let out = ok(d, &["query", "cyber.graph", "MATCH (h:Host) RETURN h.id LIMIT 10"]);
    assert!(out.contains("host-001") && out.contains("(1 row"), "{out}");

    let out = ok(
        d,
        &[
            "--json",
            "query",
            "cyber.graph",
            "MATCH (h:Host)-[e]->(m) WHERE h.id = $id RETURN m.family, e.confidence",
            "-p",
            "id=host-001",
        ],
    );
    let v: serde_json::Value = serde_json::from_str(&out).expect("json");
    assert_eq!(v["rows"][0][0], "ExampleFamily");
    assert_eq!(v["columns"][1], "e.confidence");

    ok(d, &["query", "cyber.graph", "CREATE (:Host {id: 'host-002', risk_score: 0.1})"]);
    let out = ok(d, &["query", "cyber.graph", "MATCH (h:Host) RETURN count(*) AS n"]);
    assert!(out.contains('2'), "{out}");

    ok(d, &["index", "create", "cyber.graph", "risk_score", "--kind", "sorted"]);
    let out = ok(d, &["explain", "cyber.graph", "MATCH (h:Host) WHERE h.risk_score > 0.5 RETURN h"]);
    assert!(out.contains("IndexRangeScan"), "{out}");
    let out = ok(d, &["index", "verify", "cyber.graph"]);
    assert!(out.contains("consistent"), "{out}");
    ok(d, &["index", "rebuild", "cyber.graph"]);
    assert!(ok(d, &["index", "list", "cyber.graph"]).contains("sorted index on nodes(risk_score)"));

    ok(d, &["export", "cyber.graph", "--format", "json", "--output", "exported.json"]);
    ok(d, &["export", "cyber.graph", "--format", "csv", "--output", "out"]);
    ok(d, &["export", "cyber.graph", "--format", "graphml", "--output", "out.graphml"]);
    ok(d, &["export", "cyber.graph", "--format", "jsonl", "--output", "out.jsonl"]);
    assert!(d.join("out.nodes.csv").exists() && d.join("out.edges.csv").exists());

    ok(d, &["import", "copy.graph", "--format", "csv", "--nodes", "out.nodes.csv", "--edges", "out.edges.csv"]);
    ok(d, &["import", "copy2.graph", "--format", "jsonl", "out.jsonl"]);
    let a = ok(d, &["--json", "query", "copy.graph", "MATCH (n) RETURN n.id ORDER BY n.id"]);
    let b = ok(d, &["--json", "query", "copy2.graph", "MATCH (n) RETURN n.id ORDER BY n.id"]);
    let rows = |s: &str| serde_json::from_str::<serde_json::Value>(s).expect("json")["rows"].clone();
    assert_eq!(rows(&a), rows(&b));
    ok(d, &["checkpoint", "cyber.graph"]);
}

#[test]
fn errors_have_exit_codes_and_json() {
    let dir = tempfile::tempdir().expect("tmp");
    let d = dir.path();
    ok(d, &["init", "g.graph"]);
    let out = gv(d, &["--json", "query", "g.graph", "MATCH (n RETURN n"]);
    assert_eq!(out.status.code(), Some(3));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).expect("json error");
    assert_eq!(err["error"]["kind"], "syntax_error");
    assert_eq!(gv(d, &["query", "missing.graph", "RETURN 1"]).status.code(), Some(6));
    std::fs::write(d.join("bad.graph"), "{\"format\":\"graphydra\",\"format_version\":\"9.0\",\"graph_id\":\"x\"}")
        .expect("w");
    assert_eq!(gv(d, &["validate", "bad.graph"]).status.code(), Some(4));
    assert_eq!(gv(d, &["query", "g.graph", "--read-only", "CREATE (:X)"]).status.code(), Some(3));
    assert_eq!(gv(d, &["frobnicate"]).status.code(), Some(2));
}

#[test]
fn shell_runs_scripts_from_stdin() {
    use std::io::Write;
    let dir = tempfile::tempdir().expect("tmp");
    let d = dir.path();
    ok(d, &["init", "s.graph"]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_graphydra"))
        .current_dir(d)
        .args(["shell", "s.graph"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"CREATE (:A {id: 'a'});\nMATCH (n)\nRETURN n.id;\n:info\n:quit\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 node(s) created"), "{text}");
    assert!(text.contains("| a") || text.contains("\na\n") || text.contains("a\n"), "{text}");
    // The write was checkpointed on exit.
    let out = ok(d, &["query", "s.graph", "MATCH (n) RETURN count(*) AS c"]);
    assert!(out.contains('1'));
}
