# HouyiDB VectorDBBench Comparison Protocol

> [中文](VDBBENCH_PROTOCOL.md)

> Goal: compare HouyiDB with external baselines using the same parameters, the
> same machine, and the same process, with results that are reproducible and
> auditable. This document defines only **how to measure** and **how to
> record**; it does not promise any specific numbers. This repository publishes
> the protocol only, not third-party comparison results.

## 1. Tooling and Prerequisites

- Benchmark tool: `VectorDBBench` (local working copy, hereinafter `VDB_ROOT`).
- Comparison system: `Qdrant`, running on the same machine.
- HouyiDB's VectorDBBench client adapter is not in this repository: point
  `VDB_ROOT` at a working copy that contains the adapter, or integrate
  manually via `--transport http`.
- Python 3.10+, a dedicated venv is recommended:

```bash
cd "$VDB_ROOT"
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -U pip
python -m pip install -e ".[test,qdrant]"
```

This repository's comparison scripts resolve `VDB_ROOT` by default to
`vectorDBBench-houyidb/` alongside `houyidb`; override explicitly when the
directory differs:

```bash
export VDB_ROOT=/path/to/VectorDBBench
```

## 2. Fairness Principles

- **Same case**: both sides use the same `case_type`.
- **Same query parameters**: same `k`, `m`, `ef_construction`, `ef_search`,
  `num_concurrency`, `concurrency_duration`.
- **Same hardware**: run on the same machine; do not let the two sides share
  load outside the CPU peak window.
- **Same process**: both execute `load -> optimize -> search_serial ->
  search_concurrent`.
- **Consistent cold start**: the HouyiDB side starts with an empty database and
  no old WAL; the Qdrant side likewise uses an empty collection.
- **Failures visible**: OOM, timeouts, and connection errors must be written
  into the results; silent retries or discards are not allowed.
- **No mixed comparisons**: results from different cases, different `k`,
  different concurrency levels, or different transports must not be placed in
  the same comparison table.

## 3. Default Parameters

The comparison scripts ship with the following defaults, overridable via
environment variables; **both sides must use the same set of values**:

| Variable | Default | Description |
|------|------|------|
| `VDB_CASE` | `Performance1536D50K` | VectorDBBench case |
| `VDB_K` | `10` | Number of results returned |
| `VDB_M` | `24` | HNSW `m` |
| `VDB_EFC` | `400` | `ef_construction` |
| `VDB_EFS` | `256` | Query `ef_search` |
| `VDB_NUM_CONCURRENCY` | `1` | Comma-separated allowed, e.g. `1,8,16` |
| `VDB_CONCURRENCY_DURATION` | `30` | Duration in seconds for each concurrency level |
| `VDB_LOAD_CONCURRENCY` | `0` | Load concurrency; `0` = number of CPU cores |
| `VDB_HOUYI_TRANSPORT` | `pgwire` | `pgwire` or `http` |
| `HOUYI_PG_PORT` | `15432` | HouyiDB pgwire port |
| `HOUYI_HTTP_PORT` | `17000` | HouyiDB HTTP port |
| `QDRANT_URL` | `http://127.0.0.1:6333` | Qdrant HTTP address |

`HOUYI_HNSW_*` build parameters (e.g. `HOUYI_HNSW_MAX_BUILD_BATCH`,
`HOUYI_HNSW_PARALLEL_BATCH_BUILD`) are server-side tuning knobs; they must be
set **before** starting `houyi-server` and recorded with the results; they must
remain constant when comparing the two sides.

## 4. Start Both Services

HouyiDB (server parameters must align with the bench's `m` / `ef_construction`):

```bash
./target/release/houyi-server \
  --benchmark-http-addr 127.0.0.1:17000 \
  --pgwire-addr 127.0.0.1:15432 \
  --vector-dim 1536 \
  --index-type hnsw \
  --pgwire-hnsw-m 24 \
  --pgwire-hnsw-efc 400 \
  --wal-path /tmp/houyidb-bench/houyidb.wal \
  --snapshot-interval-secs 0
```

`--snapshot-interval-secs 0` disables periodic graph snapshots so background
snapshots do not affect the write phase; if you need to validate the snapshot
path, run a separate set with annotated parameters.

Qdrant (native binary or container):

```bash
# Confirm Qdrant is reachable after starting it
curl -fsS http://127.0.0.1:6333/collections
```

For local access, set `NO_PROXY=127.0.0.1,localhost` to avoid system proxy
hijacking requests.

## 5. One-Shot Run

The public adapter lives in `benchmarks/vectordb_bench_houyidb/` and uses the
same defaults as this document:

```bash
# 1) Install the adapter into a local VectorDBBench checkout
python3 benchmarks/vectordb_bench_houyidb/install.py --vdb-root "$VDB_ROOT"

# 2) Start houyi-server (empty WAL), then run the benchmark
VDB_ROOT="$VDB_ROOT" ./benchmarks/vectordb_bench_houyidb/run_bench.sh

# 3) Optionally compare two result JSON files (with recall/ndcg gates)
python3 scripts/vdbbench_compare_results.py <reference.json> <houyidb.json> \
  --min-recall 0.95
```

The script checks HouyiDB `GET /v1/health` before running; start and verify any
external baseline with its own client.

It fails immediately and prints the target address when unreachable, avoiding
empty runs.

## 6. Interface Path Annotation

Every result must state the data-plane interface; numbers from different paths
must not be mixed in one column:

| transport | Data plane | Description |
|-----------|--------|------|
| `benchmark-http` | HTTP `/v1/vectors/*` | JSON encode/decode; control plane and data plane share a port |
| `pgwire` | PostgreSQL protocol | Bulk writes use Extended Query; queries use text/binary parameters depending on the server implementation |
| `pgwire-binary` | PostgreSQL binary parameters | Only labeled when the adapter explicitly uses binary binding |
| `adbc` | Arrow bulk transfer | No official adapter yet; results, if any, must be listed separately |

When comparing across interfaces, state whether extra JSON encoding/decoding is
involved, whether parameters are text or binary, and whether bulk write/query
optimizations are used. Conclusions based solely on benchmark HTTP must not be
presented as "HouyiDB's final protocol performance".

## 7. Result Recording Rules

Each run must save at least:

- The raw VectorDBBench `result_*.json` (uncropped);
- A summary table: case, recall@k, ndcg@k, p50/p95/p99,
  `qps`, `load_duration`, `insert_duration`, `optimize_duration`;
- Key parameters: `m` / `ef_construction` / `ef_search` / concurrency / duration;
- Server and build information: HouyiDB version or commit, Qdrant version,
  `transport`, `HNSW_*` environment variables, CPU/memory/filesystem;
- Failures and anomalies: OOM, timeouts, retry counts, log excerpts.

Metric definitions:

- `load_duration` = insert + optimize; looking at `insert_duration` alone is
  skewed because async graph construction transfers work, so it cannot be
  compared directly with older versions;
- `recall` / `ndcg` follow the ground truth produced by VectorDBBench;
- For streaming cases, the phase timestamp is "the moment the search starts
  relative to the start of writes", not the search duration of that phase;
  write speed is measured by `insert_duration`.

## 8. HNSW Layer Density and Graph Construction Path

- `HOUYI_HNSW_LAYER0_MULT`: level-0 connectivity multiplier. Default `2`
  (recall first); `1` provides higher QPS with slightly lower recall. The two
  modes cannot be switched directly on the same old graph snapshot; before
  switching, confirm the snapshot/WAL is intact or accept a full replay.
- `HOUYI_HNSW_PLAN_APPLY`: graph merging defaults to "plan under read lock +
  commit under short write lock"; searches can concurrently read the graph
  during construction; set to `0` to fall back to the old whole-graph-lock
  path. Comparative tests must use the same switch on both sides and record it
  in the results.
- Any HNSW tuning must re-run the full `load -> optimize -> search` under the
  same set of `VDB_*` parameters; do not compare only single-point
  microbenchmarks.

## 9. Reproduction and Regression

- Self-test in this repository: `./scripts/perf_smoke_fast.sh` and
  `benchmarks/vectordb_bench_houyidb/`;
- Keep the raw JSON and logs of failed runs; after a fix is merged, re-run with
  the same parameters;
- Result reports should give both absolute numbers and relative ratios, and
  state the machine specifications and versions.
