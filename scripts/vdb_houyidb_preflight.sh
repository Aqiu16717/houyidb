#!/usr/bin/env bash
# 跑 VectorDB Houyi 侧（或本地对标）**之前**调用：占端口则停服，并清本机 dev WAL，保证与「空库冷启」可复现。
# Run **before** VDB Houyi: free benchmark HTTP + pgwire ports, stop houyi-server, clear dev WAL for aligned cold start.
#
# 用法 / Usage:
#   ./scripts/vdb_houyidb_preflight.sh
#   HOUYI_HTTP_PORT=17000 HOUYI_PG_PORT=15432 HOUYI_WAL_PATH=/path/to/x.wal ./scripts/vdb_houyidb_preflight.sh
#
# 环境变量:
#   HOUYI_HTTP_PORT  默认 17000（与 vdbbench_qdrant_houyidb_gate.sh 一致）
#   HOUYI_PG_PORT     默认 15432
#   HOUYI_WAL_PATH   若设置则删除该文件及 houyi-persistence 旁路文件；空则只删 `tempfile.gettempdir()/houyidb-dev.*`
#   VDB_PREFLIGHT_NO_CLEAR_WAL=1  仅停服务，不删 WAL（调试用）
#
# EN: Kills processes listening on the two ports, pkill leftover houyi-server, removes WAL + sidecar files
#     so the next `houyi-server` start matches a clean VDB "drop_old" run.

set -euo pipefail
HTTP_PORT="${HOUYI_HTTP_PORT:-17000}"
PG_PORT="${HOUYI_PG_PORT:-15432}"

echo "==> vdb preflight: HTTP port=${HTTP_PORT} pgwire port=${PG_PORT}"

_free_port() {
  local p="$1"
  local pids
  pids=$(lsof -nP -iTCP:"$p" -sTCP:LISTEN -t 2>/dev/null || true)
  if [[ -n "${pids}" ]]; then
    # 有意省略引号：pids 为空格分隔的本地列表
    # EN: Quotes intentionally omitted; pids is a local space-separated list.
    # shellcheck disable=SC2086
    echo "    Killing listener(s) on port ${p}: ${pids}"
    kill ${pids} 2>/dev/null || true
    sleep 0.3
  else
    echo "    No listener on port ${p}"
  fi
}

_free_port "${HTTP_PORT}"
_free_port "${PG_PORT}"

# 残留 binary 名（可能未只监听上述端口，或已退出监听）
# EN: Catch stray houyi-server from prior terminal.
# 仅匹配可执行名，避免误杀其他进程；若端口已空则多为 no-op。
if pkill -x houyi-server 2>/dev/null; then
  echo "    pkill: houyi-server"
  sleep 0.3
else
  echo "    pkill -x: no houyi-server (or already gone)"
fi

if [[ "${VDB_PREFLIGHT_NO_CLEAR_WAL:-0}" == "1" ]]; then
  echo "==> vdb preflight: skip WAL clear (VDB_PREFLIGHT_NO_CLEAR_WAL=1)"
  exit 0
fi

_clear_wal_cluster() {
  local wal="$1"
  local parent dir stem
  if [[ -z "$wal" ]]; then
    return 0
  fi
  if [[ -f "$wal" ]] || [[ -e "$wal" ]]; then
    echo "    rm -f $wal"
    rm -f "$wal"
  fi
  dir=$(dirname -- "$wal")
  stem=$(basename -- "$wal" .wal)
  if [[ -n "$stem" && "$stem" != "$wal" ]]; then
    rm -f "${dir}/${stem}.applied_lsn" 2>/dev/null || true
  fi
  # Rust: `wal_path.with_extension("wal.next_lsn")` → 例如 `houyidb-dev.wal.next_lsn`
  # EN: Rust: `wal_path.with_extension("wal.next_lsn")` -> e.g.
  # EN: `houyidb-dev.wal.next_lsn`
  rm -f "${wal}.next_lsn" "${dir}/${stem}.wal.next_lsn" 2>/dev/null || true
}

if [[ -n "${HOUYI_WAL_PATH:-}" ]]; then
  echo "==> vdb preflight: clear custom WAL: ${HOUYI_WAL_PATH}"
  _clear_wal_cluster "${HOUYI_WAL_PATH}"
else
  local_td="$(python3 -c "import tempfile; print(tempfile.gettempdir())" 2>/dev/null || echo /tmp)"
  w="${local_td}/houyidb-dev.wal"
  echo "==> vdb preflight: clear default dev WAL (temp) ${w} (+ sidecars)"
  _clear_wal_cluster "$w"
  rm -f "${local_td}/houyidb-dev.applied_lsn" "${local_td}/houyidb-dev.wal.next_lsn" 2>/dev/null || true
fi

echo "==> vdb preflight: done (ports freed; WAL cleared for cold start)."
