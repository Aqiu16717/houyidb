# HouyiDB Adapter for VectorDBBench

> [简体中文](README.md)

This directory contains a publicly reproducible installer for the HouyiDB
adapter. It injects `houyidb/` into a local VectorDBBench checkout and
registers `DB.HouyiDB`, the three configuration factories, and the
`vectordb_bench houyidb` subcommand. The adapter is not published as a
standalone pip package, and it does not copy or bundle VectorDBBench itself.

## 1. Compatibility

- VectorDBBench `0.1.x` (upstream `main` layout from around July 2026).
- Python `>= 3.11`; VectorDBBench still uses `pydantic < 2`.
- The adapter relies on the `DB` enum, the `init_cls` / `config_cls` /
  `case_config_cls` factories, and the registration pattern in
  `vectordb_bench/cli/vectordbbench.py`.
- `install.py` verifies these extension points before writing any file. If an
  upstream change removes or reshapes them, it stops with an explicit error
  instead of guessing.
- Verification scope: installation, repeated installation (idempotency),
  registration, and Python syntax. No performance number is claimed here.

## 2. Installation

Prepare a VectorDBBench checkout first:

```bash
git clone https://github.com/zilliztech/VectorDBBench.git /path/to/VectorDBBench
cd /path/to/VectorDBBench
python3 -m venv .venv
source .venv/bin/activate
python -m pip install -U pip
python -m pip install -e .
python -m pip install "psycopg[binary]>=3.1" requests
```

Then run the installer from the HouyiDB repository:

```bash
cd /path/to/houyidb
python3 benchmarks/vectordb_bench_houyidb/install.py \
  --vdb-root /path/to/VectorDBBench
```

The installer:

1. copies `houyidb/` to
   `$VDB_ROOT/vectordb_bench/backend/clients/houyidb/`;
2. inserts `HouyiDB = "HouyiDB"` and all three factory branches into
   `backend/clients/__init__.py`;
3. inserts the import and `cli.add_command(HouyiDB)` into
   `cli/vectordbbench.py`;
4. runs `compileall` to validate the installation.

Runs are idempotent: existing enum, factory branches, and CLI registrations
are detected and skipped. On failure, the original registration files and any
previous adapter directory are retained under
`$VDB_ROOT/.houyidb-install-backup/<timestamp>/`, and the installer prints
explicit rollback commands.

Smoke check:

```bash
cd /path/to/VectorDBBench
source .venv/bin/activate
vectordb_bench houyidb --help
```

HTTP transport needs `requests`; pgwire transport additionally needs
`psycopg[binary]>=3.1`.

## 3. Start houyidb-server

Build and start the server from the HouyiDB repository. The ports below match
the benchmark defaults in
[`VDBBENCH_PROTOCOL.en.md`](../../VDBBENCH_PROTOCOL.en.md):

```bash
cargo build --release -p houyi-server

# Remove the old WAL for a cold start; local development directory only
rm -rf /tmp/houyidb-bench

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

Check readiness:

```bash
curl -fsS http://127.0.0.1:17000/v1/health
# {"status":"ok"}
```

Notes:

- Keep `--pgwire-hnsw-m` / `--pgwire-hnsw-efc` aligned with the benchmark
  `--m` / `--ef-construction`.
- The adapter cannot drop server-side data. `--drop-old` is passed by
  VectorDBBench but has no server-side delete effect; start from a fresh WAL
  and a fresh server process for every cold comparison.
- Set `NO_PROXY=127.0.0.1,localhost` when running locally to avoid proxy
  interception.
- HNSW server-side tuning variables (for example
  `HOUYI_HNSW_MAX_BUILD_BATCH`) must be set before starting `houyi-server` and
  recorded with the result.

## 4. One-command run

`run_bench.sh` uses the protocol defaults:

```bash
export VDB_ROOT=/path/to/VectorDBBench
cd /path/to/houyidb
./benchmarks/vectordb_bench_houyidb/run_bench.sh
```

The script:

- validates `VDB_ROOT` and the installed adapter;
- activates `$VDB_ROOT/.venv/bin/activate` when no environment is active;
- checks `http://$HOUYI_HOST:$HOUYI_HTTP_PORT/v1/health`;
- calls `vectordb_bench houyidb` with `Performance1536D50K`, `k=10`, `m=24`,
  `ef_construction=400`, `ef_search=256`, concurrency `1`, and duration `30s`;
- prints the newest result JSON path after the run.

Environment overrides:

| Variable | Default | Description |
|----------|---------|-------------|
| `VDB_ROOT` | required | VectorDBBench source checkout |
| `HOUYI_HOST` | `127.0.0.1` | HouyiDB host |
| `HOUYI_PG_PORT` | `15432` | pgwire port |
| `HOUYI_HTTP_PORT` | `17000` | HTTP port |
| `VDB_HOUYI_TRANSPORT` | `pgwire` | `pgwire` or `http` |
| `VDB_CASE` | `Performance1536D50K` | VectorDBBench case |
| `VDB_K` | `10` | Top-k |
| `VDB_M` | `24` | HNSW `m` |
| `VDB_EFC` | `400` | `ef_construction` |
| `VDB_EFS` | `256` | `ef_search` |
| `VDB_NUM_CONCURRENCY` | `1` | Concurrent search workers |
| `VDB_CONCURRENCY_DURATION` | `30` | Seconds per concurrency level |
| `VDB_LOAD_CONCURRENCY` | `0` | Load workers; `0` means CPU count |
| `VDB_METRIC` | `COSINE` | `L2`, `COSINE`, or `IP` |
| `HOUYI_TIMEOUT_SEC` | `600` | Per-request timeout in seconds |
| `HOUYI_USE_TLS` | `0` | Set `1` to use HTTPS for HTTP requests |
| `HOUYI_API_KEY` | empty | Optional authentication token |
| `TASK_LABEL` | `houyidb-<timestamp>` | Result task label |

Extra command-line arguments are appended to `vectordb_bench`, for example:

```bash
./benchmarks/vectordb_bench_houyidb/run_bench.sh --case-type Performance1536D50K
```

## 5. Direct CLI usage

Without the helper script:

```bash
source /path/to/VectorDBBench/.venv/bin/activate
export NO_PROXY=127.0.0.1,localhost

vectordb_bench houyidb \
  --host 127.0.0.1 \
  --port 15432 \
  --http-port 17000 \
  --transport pgwire \
  --case-type Performance1536D50K \
  --k 10 \
  --m 24 \
  --ef-construction 400 \
  --ef-search 256 \
  --metric-type COSINE \
  --load-concurrency 0 \
  --num-concurrency 1 \
  --concurrency-duration 30 \
  --task-label "houyidb-$(date +%Y%m%d-%H%M%S)"
```

Adapter options:

| Option | Default | Description |
|--------|---------|-------------|
| `--host` | required | HouyiDB host |
| `--port` | `15432` | pgwire port |
| `--http-port` | `17000` | HTTP port; also the data port for `--transport http` |
| `--transport` | `pgwire` | `pgwire` or `http` |
| `--use-tls` | off | Use HTTPS for HTTP requests |
| `--api-key` | empty | Auth token; `HOUYI_API_KEY` is also supported |
| `--timeout-sec` | `600` | Per-request timeout |
| `--metric-type` | `COSINE` | `L2`, `COSINE`, or `IP` |
| `--m` | `24` | HNSW `m` |
| `--ef-construction` | `400` | HNSW `ef_construction` |
| `--ef-search` | `256` | HNSW `ef_search` |

Other options (`--case-type`, `--k`, `--num-concurrency`,
`--concurrency-duration`, `--task-label`, and so on) come from the shared
VectorDBBench CLI.

The `mock` transport is retained only for offline adapter self-tests. It is
never used by `run_bench.sh` and must not be used to publish performance or
recall numbers.

## 6. Transports and interface paths

| transport | Writes | Searches | Result label |
|-----------|--------|----------|--------------|
| `pgwire` | PostgreSQL extended query + `BYTEA` binary batches | `BYTEA` query parameter; label filters fall back to HTTP | `pgwire-binary` |
| `http` | `POST /v1/vectors/upsert` JSON | `POST /v1/vectors/search` JSON | `benchmark-http` |

Both transports use the HTTP control plane:

- `GET /v1/health`: health check;
- `POST /v1/optimize`: trigger freeze and index drain;
- `GET /v1/index/status`: poll `pending_count` until it reaches zero.

Do not mix QPS, latency, or recall from different transports in one comparison
table. Every result must state its data-plane path. See
[`VDBBENCH_PROTOCOL.en.md`](../../VDBBENCH_PROTOCOL.en.md) for the full
methodology.

Authentication:

- HTTP: `--api-key` / `HOUYI_API_KEY` is sent as
  `Authorization: Bearer <token>`;
- pgwire: the token is sent as the connection password. The pgwire password
  channel is plaintext; use it only on localhost or a trusted network;
- prefer environment variables over command-line tokens to avoid shell history
  and process-list exposure.

## 7. Recording results

Raw VectorDBBench results are written to:

```text
$VDB_ROOT/vectordb_bench/results/HouyiDB/result_*.json
```

For each run, retain:

- the uncropped `result_*.json`;
- `task_label`, case, `k`, `m`, `ef_construction`, `ef_search`, concurrency,
  and duration;
- transport, HouyiDB commit/version, the full `houyi-server` command line, and
  relevant `HOUYI_HNSW_*` environment variables;
- `recall`, `ndcg`, QPS, p50/p95/p99, `load_duration`, `insert_duration`, and
  `optimize_duration`;
- failures: OOM, timeout, connection errors, retries, and relevant log lines.

`load_duration` covers insert plus index drain. With asynchronous build it is
not valid to compare `insert_duration` alone. Retain raw JSON and logs for
failed runs as well.

## 8. Troubleshooting

- **`No such command 'houyidb'` or `Unknown DB: HouyiDB`**: the adapter is not
  installed, or registration files were overwritten; rerun `install.py`.
- **`transport=pgwire requires psycopg`**: install `psycopg[binary]>=3.1` in
  the VectorDBBench virtual environment.
- **Health check fails**: make sure `--http-port` points to
  `--benchmark-http-addr`, not the pgwire port, and inspect `NO_PROXY`.
- **Empty results or bad recall**: verify that the server did not reuse an old
  WAL, that `--vector-dim` matches the case, and that `--pgwire-hnsw-m` /
  `--pgwire-hnsw-efc` match the CLI options.
- **Installer stops**: restore both registration files from the printed backup
  path, then confirm the VectorDBBench version.

## 9. Uninstall

The installer has no automatic uninstall. On a clean VectorDBBench checkout:

```bash
rm -rf "$VDB_ROOT/vectordb_bench/backend/clients/houyidb"
git -C "$VDB_ROOT" checkout -- \
  vectordb_bench/backend/clients/__init__.py \
  vectordb_bench/cli/vectordbbench.py
```

If the checkout has other uncommitted changes, do not run `git checkout`.
Instead, remove the `DB.HouyiDB` enum entry, the three factory branches, and
the CLI registration manually.

## 10. Source and license

The adapter is maintained by the HouyiDB project and released under
Apache-2.0; see [`LICENSE`](../../LICENSE). It targets the public client and
CLI extension points of VectorDBBench. VectorDBBench is an independent Zilliz
project and is used under its MIT license. This directory installs only the
adapter and does not redistribute VectorDBBench.
