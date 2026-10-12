//! Sample data so a first-time user has something to explore.

use graphydra_core::{Graph, IndexDef, IndexKind, IndexTarget, Result};

const SCRIPT: &str = r#"
CREATE (:Host:Server {id: "web-01", hostname: "web-01", os: "linux", ip: "10.0.0.10"});
CREATE (:Host:Server {id: "db-01", hostname: "db-01", os: "linux", ip: "10.0.0.20"});
CREATE (:Host:Workstation {id: "ws-07", hostname: "ws-07", os: "windows", ip: "10.0.1.7"});
CREATE (:User {id: "u-alice", name: "alice", department: "finance"});
CREATE (:User {id: "u-bob", name: "bob", department: "engineering"});
CREATE (:Process {id: "p-1", name: "powershell.exe", pid: 4120});
CREATE (:Process {id: "p-2", name: "rundll32.exe", pid: 5532});
CREATE (:File {id: "f-1", name: "invoice.docm", sha256: "9f2c..."});
CREATE (:Domain {id: "d-1", name: "update-cdn.example", reputation: "malicious"});
CREATE (:Vulnerability {id: "CVE-2024-3400", name: "CVE-2024-3400", cvss: 10.0});
CREATE (:ThreatActor {id: "ta-1", name: "APT-Example", origin: "unknown"});
MATCH (u {id: "u-alice"}), (h {id: "ws-07"}) CREATE (u)-[:LOGGED_INTO {at: "2026-10-01T08:12:00Z"}]->(h);
MATCH (u {id: "u-bob"}), (h {id: "web-01"}) CREATE (u)-[:LOGGED_INTO {at: "2026-10-01T09:00:00Z"}]->(h);
MATCH (u {id: "u-alice"}), (f {id: "f-1"}) CREATE (u)-[:OPENED]->(f);
MATCH (f {id: "f-1"}), (p {id: "p-1"}) CREATE (f)-[:SPAWNED]->(p);
MATCH (h {id: "ws-07"}), (p {id: "p-1"}) CREATE (h)-[:RAN]->(p);
MATCH (p {id: "p-1"}), (q {id: "p-2"}) CREATE (p)-[:SPAWNED]->(q);
MATCH (p {id: "p-2"}), (d {id: "d-1"}) CREATE (p)-[:CONNECTED_TO {port: 443}]->(d);
MATCH (h {id: "ws-07"}), (s {id: "db-01"}) CREATE (h)-[:CONNECTED_TO {port: 5432}]->(s);
MATCH (h {id: "web-01"}), (v {id: "CVE-2024-3400"}) CREATE (h)-[:HAS_VULNERABILITY]->(v);
MATCH (t {id: "ta-1"}), (v {id: "CVE-2024-3400"}) CREATE (t)-[:EXPLOITS]->(v);
MATCH (t {id: "ta-1"}), (d {id: "d-1"}) CREATE (t)-[:OPERATES]->(d);
"#;

pub fn populate(g: &mut Graph) -> Result<()> {
    g.transaction(|g| {
        for statement in SCRIPT.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            g.run(statement)?;
        }
        g.set_metadata("description", "GrapHydra Studio sample: a small security incident graph")
    })?;
    g.create_index(IndexDef::new(IndexKind::Hash, IndexTarget::Node, "hostname"))?;
    g.create_index(IndexDef::new(IndexKind::Text, IndexTarget::Node, "name"))?;
    g.checkpoint()
}
