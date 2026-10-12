# Performance

These numbers are measurements, not guarantees. They depend on hardware,
data shape, cache state and machine load.

## Method

`crates/graphydra-core/examples/bench.rs` generates a deterministic graph:
* a fixed-seed xorshift RNG;
* 3 labels and 3 node properties (one float, one integer bucket, one text);
* 2 edge types;
* skewed edge sources, which create hub nodes.

It then measures, in a release build with `Durability::Full` (fsync on):

* **Ingest:** batches of 50,000 records, one transaction (one fsync) per batch.
* **Index build:** a hash, a sorted and a text index.
* **Checkpoint:** writing the snapshot and fsyncing it.
* **Cold open:** reading and validating the snapshot and rebuilding every index.
* **Query latency:** median, p95 and p99 over 10–2,000 runs per query, with
  random parameters and 20 warm-up runs. Times include parsing (statement
  cache), planning, execution and result materialization.
* **Peak resident memory** of the benchmark process.

## Reproduce

```bash
cargo run --release -p graphydra-core --example bench -- 100000 500000 --json docs/benchmarks/results-100k-500k.json
python scripts/plot_bench.py docs/benchmarks/results-100k-500k.json
```

The second command writes light and dark PNG charts and a Markdown table
next to the JSON file. Run the spec's larger sizes the same way, given
enough free RAM (peak memory is roughly 1.1 KiB per record, i.e. about 6.5 GB for
1M nodes + 5M edges at these property sizes):

```bash
cargo run --release -p graphydra-core --example bench -- 1000000 5000000 --json docs/benchmarks/results-1m-5m.json
cargo run --release -p graphydra-core --example bench -- 10000000 50000000 --json docs/benchmarks/results-10m-50m.json
```

## Results: 100,000 nodes, 500,000 edges

Hardware: Intel Core i5-1145G7 (4 cores, 2.6 GHz), 16 GB RAM, Windows 11.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="benchmarks/results-100k-500k-latency-dark.png">
  <img alt="Query latency chart" src="benchmarks/results-100k-500k-latency-light.png">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="benchmarks/results-100k-500k-phases-dark.png">
  <img alt="Ingest, checkpoint and cold open chart" src="benchmarks/results-100k-500k-phases-light.png">
</picture>

The table behind the charts is in
[benchmarks/results-100k-500k.md](benchmarks/results-100k-500k.md), and the
raw data in [benchmarks/results-100k-500k.json](benchmarks/results-100k-500k.json).

**Run-to-run variance.** The machine had about 1 GB of free memory during
this run. Repeated runs on the same machine varied by up to 2× for
scan-heavy and I/O-heavy phases:

| | Range across runs |
|---|---|
| cold open | 2.0–3.7 s |
| full-scan queries | 33–51 ms median |

Point lookups and indexed queries were stable within about ±30%.

### Against the provisional targets in the design document

| Target | Measured (median) | Status |
|---|---|---|
| Indexed node lookup < 1 ms | 0.007 ms | met |
| Indexed property lookup < 5 ms | 0.06 ms (hash), 0.10 ms (range) | met |
| Local one-hop traversal < 5 ms | 0.02 ms | met |
| Parse/plan measured separately | `QueryStats.parse_time` / `plan_time` / `execution_time` | done |
| Cold open, index build, recovery measured separately | see the phases chart; recovery covered by tests | done |
| 1M / 5M and 10M / 50M runs | runnable with the commands above; not run here (insufficient free RAM) | pending |

## What dominates cost

* **Full scans** (unindexed predicates, label scans with aggregation) are
  O(V). Create an index; `EXPLAIN` names the missing one.
* **Text matching** re-checks every candidate against the original predicate
  (the correctness guarantee), so the cost is proportional to the posting
  list of the rarest query term.
* **Cold open** parses the whole JSON snapshot and rebuilds every index, so
  it is O(V + E). A binary, memory-mapped backend is the planned next step if
  open time matters at 10M+ scale. The storage layer is isolated to allow it.
* **Memory** peaked at about 1.1 KiB per record (node or edge) here, including
  property maps, three secondary indexes and allocator overhead.
