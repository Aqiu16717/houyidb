#!/usr/bin/env bash
# Wrapper: 处理 HouyiDB 的 vector_dim 固定问题，分维度分别启动 HouyiDB 再测试
# EN: Wrapper: works around HouyiDB's fixed vector_dim by starting HouyiDB per
# EN: dimension and then testing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VDB_ROOT="${VDB_ROOT:-$(cd "${ROOT}/.." && pwd)/vectorDBBench-houyidb}"
HOUYI_BIN="${ROOT}/target/release/houyi-server"
HTTP_ADDR="127.0.0.1:17000"
PG_ADDR="127.0.0.1:15432"

NO_PROXY="${NO_PROXY:-127.0.0.1,localhost}"
export NO_PROXY
export no_proxy="${NO_PROXY:-127.0.0.1,localhost}"

DB="${1:-houyidb}"
SUITE="${2:-all}"

BENCH_SCRIPT="${ROOT}/scripts/run_full_benchmark.py"

_run_venv() {
  cd "${VDB_ROOT}"
  if [[ ! -d .venv ]]; then
    echo "ERROR: ${VDB_ROOT}/.venv missing" >&2
    exit 1
  fi
  source .venv/bin/activate
}

_start_houyi() {
  local dim="$1"
  echo "==> Starting HouyiDB with vector_dim=${dim}"
  pkill -f houyi-server 2>/dev/null || true
  sleep 1
  rm -f /var/folders/ym/lpc4tq_x41q4h3tdx7vmsy7m0000gn/T/houyidb-dev.wal
  RUST_LOG=info "${HOUYI_BIN}" \
    --benchmark-http-addr "${HTTP_ADDR}" \
    --pgwire-addr "${PG_ADDR}" \
    --vector-dim "${dim}" \
    --pgwire-hnsw-m 24 \
    --pgwire-hnsw-efc 400 \
    --wal-write-mode async \
    > /tmp/houyidb-bench.log 2>&1 &
  sleep 3
  if ! curl -s "http://${HTTP_ADDR}/v1/health" > /dev/null 2>&1; then
    echo "ERROR: HouyiDB failed to start" >&2
    cat /tmp/houyidb-bench.log | tail -20 >&2
    exit 1
  fi
  echo "    HouyiDB (dim=${dim}) healthy"
}

_stop_houyi() {
  echo "==> Stopping HouyiDB"
  pkill -f houyi-server 2>/dev/null || true
  sleep 1
}

_start_qdrant() {
  echo "==> Starting Qdrant"
  pkill -f qdrant 2>/dev/null || true
  sleep 1
  ~/Downloads/qdrant --uri http://127.0.0.1:6333 > /tmp/qdrant-bench.log 2>&1 &
  sleep 3
  if ! curl -s "http://127.0.0.1:6333/collections" > /dev/null 2>&1; then
    echo "ERROR: Qdrant failed to start" >&2
    exit 1
  fi
  echo "    Qdrant healthy"
}

_stop_qdrant() {
  echo "==> Stopping Qdrant"
  pkill -f qdrant 2>/dev/null || true
  sleep 1
}

run_houyidb() {
  _run_venv

  # 阶段 1：Cohere 768d 测试
  # EN: Phase 1: Cohere 768d tests
  _start_houyi 768
  echo ""
  echo "=== PHASE 1: Cohere 768d tests ==="
  python "${BENCH_SCRIPT}" --db houyidb --suite "${SUITE}" --dimension 768 2>&1 | tee /tmp/houyidb-cohere-bench.log
  _stop_houyi

  # 阶段 2：OpenAI 1536d 测试
  # EN: Phase 2: OpenAI 1536d tests
  _start_houyi 1536
  echo ""
  echo "=== PHASE 2: OpenAI 1536d tests ==="
  python "${BENCH_SCRIPT}" --db houyidb --suite "${SUITE}" --dimension 1536 2>&1 | tee /tmp/houyidb-openai-bench.log
  _stop_houyi

  echo ""
  echo "=== HouyiDB benchmark complete ==="
}

run_qdrant() {
  _run_venv
  _start_qdrant
  echo ""
  echo "=== Qdrant benchmark (all dimensions in one run) ==="
  python "${BENCH_SCRIPT}" --db qdrant --suite "${SUITE}" 2>&1 | tee /tmp/qdrant-bench.log
  _stop_qdrant
  echo ""
  echo "=== Qdrant benchmark complete ==="
}

case "${DB}" in
  houyidb) run_houyidb ;;
  qdrant) run_qdrant ;;
  *)
    echo "Usage: $0 <houyidb|qdrant> [suite]" >&2
    exit 1
    ;;
esac
