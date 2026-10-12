//! Reproducible benchmark (spec section 14).
//!
//! ```text
//! cargo run --release -p graphydra-core --example bench -- 100000 500000
//! ```
//!
//! Generates a deterministic graph (power-law-ish out-degree, 3 labels,
//! 2 edge types), then measures ingestion, checkpoint, cold open, and query
//! latency (median / p95 / p99 over many runs with random parameters).

use std::time::{Duration, Instant};

use graphydra_core::{
    Config, Durability, Edge, Graph, IndexDef, IndexKind, IndexTarget, Node, OpenOptions, QueryOptions, Value,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Peak resident set size of this process, in MiB (where supported).
#[allow(unsafe_code)]
fn peak_rss_mib() -> Option<f64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: the counters struct is plain data sized as the API expects.
        unsafe {
            let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0 {
                return Some(c.PeakWorkingSetSize as f64 / 1048576.0);
            }
        }
        None
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        let kb: f64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb / 1024.0)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        None
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx]
}

/// Results collected for `--json` output (rendered by scripts/plot_bench.py).
#[derive(Default)]
struct Report {
    phases: Vec<(String, f64)>,
    queries: Vec<(String, f64, f64, f64, f64)>,
}

#[allow(clippy::too_many_arguments)]
fn bench(
    report: &mut Report,
    g: &mut Graph,
    name: &str,
    iterations: usize,
    rng: &mut Rng,
    nodes: u64,
    query: &str,
    param: impl Fn(&mut Rng, u64) -> Value,
) {
    let mut times = Vec::with_capacity(iterations);
    let mut rows = 0usize;
    for i in 0..iterations + 20 {
        let opts = QueryOptions::new().param("p", param(rng, nodes));
        let start = Instant::now();
        let r = g.query(query, &opts).expect("benchmark query");
        let t = start.elapsed();
        if i >= 20 {
            // first 20 runs are warm-up
            times.push(t);
            rows += r.rows.len();
        }
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    report.queries.push((
        name.to_owned(),
        ms(percentile(&times, 0.5)),
        ms(percentile(&times, 0.95)),
        ms(percentile(&times, 0.99)),
        rows as f64 / iterations as f64,
    ));
    println!(
        "| {name:<44} | {:>9.3} | {:>9.3} | {:>9.3} | {:>8.1} |",
        percentile(&times, 0.5).as_secs_f64() * 1e3,
        percentile(&times, 0.95).as_secs_f64() * 1e3,
        percentile(&times, 0.99).as_secs_f64() * 1e3,
        rows as f64 / iterations as f64
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let m: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5 * n);
    let dir = std::env::temp_dir().join(format!("graphydra-bench-{n}-{m}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("bench dir");
    let path = dir.join("bench.graph");
    let config = Config { durability: Durability::Full, checkpoint_journal_bytes: u64::MAX, ..Config::default() };

    println!("## GrapHydra benchmark: {n} nodes, {m} edges\n");
    println!("os: {} / {}, graphydra {}", std::env::consts::OS, std::env::consts::ARCH, graphydra_core::VERSION);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let mut g = Graph::open_with(&path, OpenOptions::new().create(true).config(config.clone())).expect("create");
    let labels = ["Host", "Malware", "Indicator"];
    let start = Instant::now();
    let batch = 50_000u64;
    let mut i = 0;
    while i < n {
        let end = (i + batch).min(n);
        g.add_nodes((i..end).map(|k| {
            Node::new(format!("n{k}"))
                .with_label(labels[(k % 3) as usize])
                .with_property("score", (k % 1000) as f64 / 1000.0)
                .with_property("bucket", (k % 997) as i64)
                .with_property("name", format!("node {k} item{}", k % 50))
        }))
        .expect("nodes");
        i = end;
    }
    let node_time = start.elapsed();
    let start = Instant::now();
    let mut j = 0;
    while j < m {
        let end = (j + batch).min(m);
        let edges: Vec<Edge> = (j..end)
            .map(|k| {
                // Skewed sources create hubs; targets are uniform.
                let hub = rng.below(n) + 1;
                let src = rng.below(hub);
                let tgt = rng.below(n);
                Edge::new(
                    format!("e{k}"),
                    format!("n{src}"),
                    format!("n{tgt}"),
                    if k % 4 == 0 { "USES" } else { "LINKS" },
                )
                .with_property("w", (k % 10) as i64)
            })
            .collect();
        g.add_edges(edges).expect("edges");
        j = end;
    }
    let edge_time = start.elapsed();
    let start = Instant::now();
    g.create_index(IndexDef::new(IndexKind::Hash, IndexTarget::Node, "bucket")).expect("idx");
    g.create_index(IndexDef::new(IndexKind::Sorted, IndexTarget::Node, "score")).expect("idx");
    g.create_index(IndexDef::new(IndexKind::Text, IndexTarget::Node, "name")).expect("idx");
    let index_time = start.elapsed();
    let start = Instant::now();
    g.checkpoint().expect("checkpoint");
    let checkpoint_time = start.elapsed();
    drop(g);
    let size = std::fs::metadata(&path).map(|md| md.len()).unwrap_or(0);

    let start = Instant::now();
    let mut g = Graph::open_with(&path, OpenOptions::new().config(config)).expect("open");
    let open_time = start.elapsed();

    let mut report = Report::default();
    for (name, t) in [
        ("ingest nodes", node_time),
        ("ingest edges", edge_time),
        ("build 3 property indexes", index_time),
        ("checkpoint (snapshot + fsync)", checkpoint_time),
        ("cold open (parse + build indexes)", open_time),
    ] {
        report.phases.push((name.to_owned(), t.as_secs_f64()));
    }
    println!("\n| phase | time | throughput |\n|---|---:|---:|");
    println!(
        "| ingest nodes (batches of {batch}, fsync per batch) | {:.2} s | {:.0} nodes/s |",
        node_time.as_secs_f64(),
        n as f64 / node_time.as_secs_f64()
    );
    println!(
        "| ingest edges (batches of {batch}, fsync per batch) | {:.2} s | {:.0} edges/s |",
        edge_time.as_secs_f64(),
        m as f64 / edge_time.as_secs_f64()
    );
    println!("| build 3 property indexes | {:.2} s | |", index_time.as_secs_f64());
    println!(
        "| checkpoint (write {:.1} MiB snapshot + fsync) | {:.2} s | |",
        size as f64 / 1048576.0,
        checkpoint_time.as_secs_f64()
    );
    println!("| cold open (read, validate, build all indexes) | {:.2} s | |", open_time.as_secs_f64());

    println!("\nQuery latency in milliseconds (after 20 warm-up runs):\n");
    println!("| {:<44} | {:>9} | {:>9} | {:>9} | {:>8} |", "query", "median", "p95", "p99", "rows");
    println!("|{:-<46}|{:->11}|{:->11}|{:->11}|{:->10}|", "", ":", ":", ":", ":");
    let id = |r: &mut Rng, n: u64| Value::from(format!("n{}", r.below(n)));
    bench(&mut report, &mut g, "node by id", 2000, &mut rng, n, "MATCH (x) WHERE x.id = $p RETURN x.name", id);
    bench(
        &mut report,
        &mut g,
        "one hop out from id",
        2000,
        &mut rng,
        n,
        "MATCH (x)-[:LINKS]->(y) WHERE x.id = $p RETURN y.id",
        id,
    );
    bench(
        &mut report,
        &mut g,
        "one hop both directions, count",
        2000,
        &mut rng,
        n,
        "MATCH (x)--(y) WHERE x.id = $p RETURN count(y)",
        id,
    );
    bench(
        &mut report,
        &mut g,
        "two hops, distinct targets",
        500,
        &mut rng,
        n,
        "MATCH (x)-->()-->(z) WHERE x.id = $p RETURN count(DISTINCT z)",
        id,
    );
    bench(
        &mut report,
        &mut g,
        "var-length *1..3 from id (LIMIT 100)",
        500,
        &mut rng,
        n,
        "MATCH (x)-[*1..3]->(z) WHERE x.id = $p RETURN z.id LIMIT 100",
        id,
    );
    bench(
        &mut report,
        &mut g,
        "hash index equality (bucket = p)",
        1000,
        &mut rng,
        n,
        "MATCH (x:Host) WHERE x.bucket = $p RETURN x.id",
        |r, _| Value::Int(r.below(997) as i64),
    );
    bench(
        &mut report,
        &mut g,
        "sorted index range (score in [p, p+0.001))",
        1000,
        &mut rng,
        n,
        "MATCH (x) WHERE x.score >= $p AND x.score < $p + 0.001 RETURN count(*)",
        |r, _| Value::Float(r.below(999) as f64 / 1000.0),
    );
    bench(
        &mut report,
        &mut g,
        "text index match",
        200,
        &mut rng,
        n,
        "MATCH (x) WHERE text_match(x.name, $p) RETURN count(*)",
        |r, _| Value::from(format!("item{}", r.below(50))),
    );
    bench(
        &mut report,
        &mut g,
        "shortest path between random nodes (*..6)",
        100,
        &mut rng,
        n,
        "MATCH p = shortestPath((a {id: $p})-[*..6]-(b {id: 'n1'})) RETURN length(p)",
        id,
    );
    bench(
        &mut report,
        &mut g,
        "label scan + aggregate (full scan)",
        10,
        &mut rng,
        n,
        "MATCH (x:Malware) RETURN avg(x.score), count(*)",
        |_, _| Value::Null,
    );
    bench(
        &mut report,
        &mut g,
        "unindexed property filter (full scan)",
        10,
        &mut rng,
        n,
        "MATCH (x) WHERE x.name ENDS WITH $p RETURN count(*)",
        |r, _| Value::from(format!("{}", r.below(10))),
    );
    drop(g);
    let peak = peak_rss_mib();
    if let Some(mib) = peak {
        println!(
            "
peak resident memory: {mib:.0} MiB (ingest + checkpoint + reopen in one process)"
        );
    }
    if let Some(out) = args.iter().position(|a| a == "--json").and_then(|i| args.get(i + 1)) {
        let json = serde_json::json!({
            "nodes": n,
            "edges": m,
            "version": graphydra_core::VERSION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "snapshot_mib": size as f64 / 1048576.0,
            "peak_rss_mib": peak,
            "phases": report.phases.iter().map(|(k, v)| serde_json::json!({"phase": k, "seconds": v})).collect::<Vec<_>>(),
            "queries": report.queries.iter().map(|(q, p50, p95, p99, rows)| serde_json::json!({
                "query": q, "median_ms": p50, "p95_ms": p95, "p99_ms": p99, "rows": rows
            })).collect::<Vec<_>>(),
        });
        std::fs::write(out, serde_json::to_string_pretty(&json).expect("json")).expect("write --json output");
        println!("wrote {out}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
