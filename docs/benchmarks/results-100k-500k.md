### 100,000 nodes, 500,000 edges (windows/x86_64, v0.1.0)

| phase | seconds |
|---|---:|
| ingest nodes | 0.34 |
| ingest edges | 2.47 |
| build 3 property indexes | 0.30 |
| checkpoint (snapshot + fsync) | 1.61 |
| cold open (parse + build indexes) | 3.72 |

| query | median ms | p95 ms | p99 ms | rows |
|---|---:|---:|---:|---:|
| node by id | 0.007 | 0.018 | 0.033 | 1.0 |
| one hop out from id | 0.018 | 0.033 | 0.053 | 3.8 |
| one hop both directions, count | 0.021 | 0.039 | 0.056 | 1.0 |
| two hops, distinct targets | 0.025 | 0.070 | 0.116 | 1.0 |
| var-length *1..3 from id (LIMIT 100) | 0.092 | 0.173 | 0.235 | 61.9 |
| hash index equality (bucket = p) | 0.057 | 0.118 | 0.147 | 33.4 |
| sorted index range (score in [p, p+0.001)) | 0.102 | 0.208 | 0.266 | 1.0 |
| text index match | 4.268 | 7.946 | 8.831 | 1.0 |
| shortest path between random nodes (*..6) | 6.502 | 23.118 | 38.686 | 1.0 |
| label scan + aggregate (full scan) | 15.783 | 17.940 | 17.940 | 1.0 |
| unindexed property filter (full scan) | 50.787 | 71.458 | 71.458 | 1.0 |

Peak resident memory: 649 MiB; snapshot size 54.8 MiB.
