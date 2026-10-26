#!/usr/bin/env bash
# 60 秒演示：写入 → 检索 → 过滤 → kill -9 → 重启恢复。
# EN: 60-second demo: upsert -> search -> filter -> kill -9 -> restart recovery.
#
# 用法 / Usage:
#   ./demo/demo.sh                 # 正常播放（带停顿，适合录屏）
#   DEMO_FAST=1 ./demo/demo.sh     # 无停顿，用于 CI 冒烟
#
# 环境变量 / Env:
#   HOUYI_SERVER_BIN   houyi-server 路径（默认 target/release/houyi-server）
#   HOUYI_DEMO_URL     HTTP 地址（默认 127.0.0.1:17000）
#   HOUYI_DEMO_PG      pgwire 地址（默认 127.0.0.1:15432）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${HOUYI_SERVER_BIN:-$ROOT/target/release/houyi-server}"
HTTP="${HOUYI_DEMO_URL:-127.0.0.1:17000}"
PG="${HOUYI_DEMO_PG:-127.0.0.1:15432}"
BASE="http://${HTTP}"
# 固定公开路径，避免录屏暴露个人临时目录。
# EN: fixed public path so recordings do not expose a personal temp directory.
DATA="${HOUYI_DEMO_DATA:-/tmp/houyidb-demo}"
LOG="${DATA}/server.log"
PID=""
rm -rf "$DATA"
mkdir -p "$DATA"

step() { printf '\n\033[1;31m%s\033[0m\n' "$*"; }
pause() { [[ "${DEMO_FAST:-0}" == "1" ]] || sleep "${1:-0.7}"; }
pretty() {
  if command -v jq >/dev/null 2>&1; then jq -c "${1:-.}"; else cat; fi
}

start_server() {
  "$BIN" \
    --benchmark-http-addr "$HTTP" \
    --pgwire-addr "$PG" \
    --vector-dim 8 \
    --wal-path "${DATA}/houyidb.wal" \
    --snapshot-interval-secs 0 \
    >"$LOG" 2>&1 &
  PID=$!
  for _ in $(seq 1 100); do
    if curl -fsS --noproxy '*' "${BASE}/v1/health" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  echo "server failed to start; log: $LOG" >&2
  tail -20 "$LOG" >&2 || true
  exit 1
}

stop_server() {
  if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
    kill -9 "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
  fi
  PID=""
}

cleanup() {
  stop_server
  rm -rf "$DATA"
}
trap cleanup EXIT

if [[ ! -x "$BIN" ]]; then
  echo "!! houyi-server not found: $BIN" >&2
  echo "   cargo build --release -p houyi-server" >&2
  exit 1
fi

step "1/5  start houyi-server (single binary)"
start_server
echo "  HTTP  ${BASE}"
echo "  WAL   ${DATA}/houyidb.wal"
pause

step "2/5  upsert 3 vectors with payload"
curl -fsS --noproxy '*' -X POST "${BASE}/v1/collections/goods/upsert" \
  -H 'content-type: application/json' -d '{
  "vectors": [
    {"id": 1, "values": [0.92,0.88,0.06,0.05,0.05,0.04,0.05,0.05], "label": "fruit",       "payload": {"name": "apple",  "price": 3}},
    {"id": 2, "values": [0.05,0.04,0.92,0.90,0.06,0.05,0.05,0.04], "label": "electronics", "payload": {"name": "laptop", "price": 999}},
    {"id": 3, "values": [0.04,0.05,0.05,0.06,0.90,0.92,0.05,0.04], "label": "book",        "payload": {"name": "rust",   "price": 45}}
  ]
}' | pretty
pause

step "3/5  vector search (k=3)"
curl -fsS --noproxy '*' -X POST "${BASE}/v1/collections/goods/search" \
  -H 'content-type: application/json' -d '{
  "vector": [0.90,0.85,0.05,0.05,0.05,0.05,0.05,0.05],
  "k": 3, "include_payload": true
}' | pretty '{ids, scores}'
pause

step "4/5  hybrid search: price < 100"
curl -fsS --noproxy '*' -X POST "${BASE}/v1/collections/goods/hybrid/search" \
  -H 'content-type: application/json' -d '{
  "vector": [0.90,0.85,0.05,0.05,0.05,0.05,0.05,0.05],
  "k": 3, "filters": [{"column": "price", "op": "lt", "value": 100}],
  "include_payload": true
}' | pretty '{ids, scores}'
pause

step "5/5  kill -9, restart, data survives (WAL replay)"
kill -9 "$PID"
wait "$PID" 2>/dev/null || true
PID=""
start_server
curl -fsS --noproxy '*' -X POST "${BASE}/v1/collections/goods/search" \
  -H 'content-type: application/json' -d '{
  "vector": [0.90,0.85,0.05,0.05,0.05,0.05,0.05,0.05],
  "k": 3, "include_payload": true
}' | pretty '{ids, scores}'
echo
echo "done: 写入的数据在 kill -9 后仍然可查。"
echo "EN: the data is still searchable after kill -9."
pause 1.2
