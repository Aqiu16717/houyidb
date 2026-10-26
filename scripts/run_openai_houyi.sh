#!/usr/bin/env bash
# OpenAI 1536d 50K 测试 — 每轮重启 HouyiDB
# EN: OpenAI 1536d 50K test — restart HouyiDB each round.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VDB_ROOT="${ROOT}/../vectorDBBench-houyidb"
HOUYI_BIN="${ROOT}/target/release/houyi-server"
WAL_PATH="/var/folders/ym/lpc4tq_x41q4h3tdx7vmsy7m0000gn/T/houyidb-dev.wal"

export NO_PROXY="127.0.0.1,localhost"
export no_proxy="127.0.0.1,localhost"
export VDB_M=24 VDB_EFC=400 VDB_K=10
export VDB_NUM_CONCURRENCY="4,8,16"
export VDB_CONCURRENCY_DURATION=30

start_houyi() {
  pkill -f houyi-server 2>/dev/null || true
  sleep 1
  rm -f "${WAL_PATH}"
  RUST_LOG=info "${HOUYI_BIN}" \
    --benchmark-http-addr 127.0.0.1:17000 \
    --pgwire-addr 127.0.0.1:15432 \
    --vector-dim 1536 \
    --pgwire-hnsw-m 24 \
    --pgwire-hnsw-efc 400 \
    --wal-write-mode async \
    > /tmp/houyidb-bench.log 2>&1 &
  sleep 3
  for i in $(seq 1 10); do
    curl -s http://127.0.0.1:17000/v1/health > /dev/null 2>&1 && return 0
    sleep 2
  done
  echo "ERROR: start failed" >&2; return 1
}

stop_houyi() { pkill -f houyi-server 2>/dev/null || true; sleep 2; }

run_one() {
  local ef="$1"
  local ts=$(date +%Y%m%d)
  export TASK_LABEL_HOUYI="houyidb-OpenAI1536d_50K-ef${ef}-${ts}"
  cd "${VDB_ROOT}" && source .venv/bin/activate
  echo "  ef=${ef} label=${TASK_LABEL_HOUYI}"
  python -m vectordb_bench.cli.vectordbbench houyidb \
    --host 127.0.0.1 --port 15432 --http-port 17000 --transport pgwire \
    --case-type Performance1536D50K --k 10 \
    --m 24 --ef-construction 400 --ef-search "${ef}" \
    --num-concurrency "4,8,16" --concurrency-duration 30 \
    --task-label "${TASK_LABEL_HOUYI}"
}

for ef in 128 256 512; do
  echo ""
  echo "===== OpenAI 1536d 50K  ef=${ef} ====="
  start_houyi
  run_one "${ef}"
  stop_houyi
done

echo ""
echo "===== All OpenAI tests complete ====="
