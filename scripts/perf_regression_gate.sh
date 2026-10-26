#!/usr/bin/env bash
# 性能回归门禁：VectorDBench（Houyi）+ 增量 HTTP upsert 压测，失败 exit 1。
# Performance regression gate: VDB (authoritative) + incremental bench; exit 1 on failure.
#
# 用法 / Usage (仓库根目录 / from repo root):
#   ./scripts/perf_regression_gate.sh vdb
#   ./scripts/perf_regression_gate.sh incremental
#   ./scripts/perf_regression_gate.sh all
#
# 阈值见 scripts/perf_regression_thresholds.json；环境变量可覆盖，见 scripts/perf_regression_check.py
#
# all：先 VDB（冷启 → 同参 1536D），再 preflight 清库、重启，再**空库**增量压测（与 INCREMENTAL_WORKLOAD 思路一致）。

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export VDB_ROOT="${VDB_ROOT:-$(cd "${ROOT}/.." && pwd)/vectorDBBench-houyidb}"
export NO_PROXY="${NO_PROXY:-127.0.0.1,localhost}"
export HOUYI_HOST="${HOUYI_HOST:-127.0.0.1}"
export HOUYI_HTTP_PORT="${HOUYI_HTTP_PORT:-17000}"
export HOUYI_PG_PORT="${HOUYI_PG_PORT:-15432}"
export HOUYI_HNSW_PARALLEL_BATCH_BUILD="${HOUYI_HNSW_PARALLEL_BATCH_BUILD:-1}"
export HOUYI_HNSW_MAX_BUILD_BATCH="${HOUYI_HNSW_MAX_BUILD_BATCH:-8192}"
export HOUYI_SERVER_VECTOR_DIM="${PERF_GATE_VECTOR_DIM:-1536}"
PERF_VECTOR_DIM="${HOUYI_SERVER_VECTOR_DIM}"
export PERF_INC_DURATION="${PERF_INC_DURATION:-20}"
export PERF_INC_RPS="${PERF_INC_RPS:-12}"
PERF_SERVER_LOG="${PERF_SERVER_LOG:-/tmp/houyidb-perf-gate-server.log}"
export PERF_INC_OUT="${PERF_INC_OUT:-/tmp/houyidb-inc-bench-$$.json}"
SERVER_PID=""

on_exit() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    echo "==> stop houyi-server pid=${SERVER_PID}" >&2
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  SERVER_PID=""
  pkill -x houyi-server 2>/dev/null || true
  bash "${ROOT}/scripts/vdb_houyidb_preflight.sh" || true
}
trap on_exit EXIT

_start_server() {
  echo "==> start houyi-server (vector_dim=${PERF_VECTOR_DIM}, log=${PERF_SERVER_LOG})..." >&2
  : > "${PERF_SERVER_LOG}"
  (
    cd "${ROOT}" || exit 1
    export NO_PROXY
    export HOUYI_SERVER_VECTOR_DIM="${PERF_VECTOR_DIM}"
    exec cargo run -p houyi-server --bin houyi-server --release -- \
      --benchmark-http-addr "127.0.0.1:${HOUYI_HTTP_PORT}" \
      --pgwire-addr "127.0.0.1:${HOUYI_PG_PORT}" \
      --vector-dim "${PERF_VECTOR_DIM}" \
      >> "${PERF_SERVER_LOG}" 2>&1
  ) &
  SERVER_PID=$!
}

_wait_health() {
  local n=0
  while [[ $n -lt 180 ]]; do
    if [[ "$(curl -sS --noproxy '*' -o /dev/null -w '%{http_code}' "http://127.0.0.1:${HOUYI_HTTP_PORT}/v1/health" 2>/dev/null || echo 0)" == "200" ]]; then
      echo "==> benchmark HTTP ready" >&2
      return 0
    fi
    n=$((n + 1))
    sleep 1
  done
  echo "ERROR: health timeout; see ${PERF_SERVER_LOG}" >&2
  return 1
}

_run_vdb_case() {
  local label="houyidb-perf-gate-vdb-$(date +%Y%m%d-%H%M%S)"
  export TASK_LABEL_HOUYI="${label}"
  (
    cd "${ROOT}"
    VDB_ROOT="${VDB_ROOT}" \
      HOUYI_HNSW_PARALLEL_BATCH_BUILD="${HOUYI_HNSW_PARALLEL_BATCH_BUILD}" \
      HOUYI_HNSW_MAX_BUILD_BATCH="${HOUYI_HNSW_MAX_BUILD_BATCH}" \
      TASK_LABEL_HOUYI="${label}" \
      NO_PROXY="${NO_PROXY}" \
      "${ROOT}/scripts/vdbbench_qdrant_houyidb_gate.sh" run-houyidb
  )
  local latest
  latest="$(ls -t "${VDB_ROOT}/vectordb_bench/results/HouyiDB"/result_*.json 2>/dev/null | head -1)" || {
    echo "ERROR: no result_*.json" >&2
    return 1
  }
  echo "==> VDB result: ${latest}" >&2
  python3 "${ROOT}/scripts/perf_regression_check.py" vdb "${latest}"
}

_cmd_vdb() {
  bash "${ROOT}/scripts/vdb_houyidb_preflight.sh"
  _start_server
  _wait_health
  _run_vdb_case
}

_run_incremental_case() {
  echo "==> incremental bench (dim=${PERF_VECTOR_DIM} duration=${PERF_INC_DURATION}s rps=${PERF_INC_RPS})" >&2
  python3 "${ROOT}/scripts/incremental_bench_http.py" \
    --base-url "http://127.0.0.1:${HOUYI_HTTP_PORT}" \
    --dim "${PERF_VECTOR_DIM}" \
    --duration "${PERF_INC_DURATION}" \
    --rps "${PERF_INC_RPS}" \
    --batch 1 > "${PERF_INC_OUT}"
  echo "==> incremental JSON: ${PERF_INC_OUT}" >&2
  python3 "${ROOT}/scripts/perf_regression_check.py" inc "${PERF_INC_OUT}"
}

_cmd_incremental() {
  bash "${ROOT}/scripts/vdb_houyidb_preflight.sh"
  _start_server
  _wait_health
  _run_incremental_case
}

_cmd_all() {
  bash "${ROOT}/scripts/vdb_houyidb_preflight.sh"
  _start_server
  _wait_health
  _run_vdb_case || return 1
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  SERVER_PID=""
  bash "${ROOT}/scripts/vdb_houyidb_preflight.sh"
  _start_server
  _wait_health
  _run_incremental_case
}

case "${1:-}" in
  vdb) _cmd_vdb ;;
  incremental) _cmd_incremental ;;
  all) _cmd_all ;;
  *)
    echo "Usage: $0 {vdb|incremental|all}" >&2
    echo "  vdb         — 仅 VDB Houyi（自启 houyi-server release，dim=\$PERF_GATE_VECTOR_DIM 默认 1536）" >&2
    echo "  incremental — 空库 + 增量 upsert 压测" >&2
    echo "  all         — 先 VDB 再清库后增量" >&2
    exit 2
    ;;
esac
